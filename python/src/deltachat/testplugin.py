from __future__ import print_function
import os
import sys
import subprocess
import queue
import threading
import fnmatch
import time
import weakref
import tempfile

import pytest
import requests

from . import Account, const, direct_imap
from .capi import lib
from .events import FFIEventLogger, FFIEventTracker
from _pytest._code import Source
from deltachat.direct_imap import ImapConn

import deltachat


def pytest_addoption(parser):
    parser.addoption(
        "--liveconfig", action="store", default=None,
        help="a file with >=2 lines where each line "
             "contains NAME=VALUE config settings for one account"
    )
    parser.addoption(
        "--ignored", action="store_true",
        help="Also run tests marked with the ignored marker",
    )


def pytest_configure(config):
    cfg = config.getoption('--liveconfig')
    if not cfg:
        cfg = os.getenv('DCC_NEW_TMP_EMAIL')
        if cfg:
            config.option.liveconfig = cfg

    # Make sure we don't get garbled output because threads keep running
    # collect all ever created accounts in a weakref-set (so we don't
    # keep objects unneccessarily alive) and enable/disable logging
    # for each pytest test phase # (setup/call/teardown).
    # Additionally make the acfactory use a logging/no-logging default.

    class LoggingAspect:
        def __init__(self):
            self._accounts = weakref.WeakSet()

        @deltachat.global_hookimpl
        def dc_account_init(self, account):
            self._accounts.add(account)

        def disable_logging(self, item):
            for acc in self._accounts:
                acc.disable_logging()
            acfactory = item.funcargs.get("acfactory")
            if acfactory:
                acfactory.set_logging_default(False)

        def enable_logging(self, item):
            for acc in self._accounts:
                acc.enable_logging()
            acfactory = item.funcargs.get("acfactory")
            if acfactory:
                acfactory.set_logging_default(True)

        @pytest.hookimpl(hookwrapper=True)
        def pytest_runtest_setup(self, item):
            if item.get_closest_marker("ignored"):
                if not item.config.getvalue("ignored"):
                    pytest.skip("use --ignored to run this test")
            self.enable_logging(item)
            yield
            self.disable_logging(item)

        @pytest.hookimpl(hookwrapper=True)
        def pytest_pyfunc_call(self, pyfuncitem):
            self.enable_logging(pyfuncitem)
            yield
            self.disable_logging(pyfuncitem)

        @pytest.hookimpl(hookwrapper=True)
        def pytest_runtest_teardown(self, item):
            self.enable_logging(item)
            yield
            self.disable_logging(item)

    la = LoggingAspect()
    config.pluginmanager.register(la)
    deltachat.register_global_plugin(la)


def pytest_report_header(config, startdir):
    summary = []

    t = tempfile.mktemp()
    try:
        ac = Account(t)
        info = ac.get_info()
        ac.shutdown()
    finally:
        os.remove(t)
    summary.extend(['Deltachat core={} sqlite={} journal_mode={}'.format(
         info['deltachat_core_version'],
         info['sqlite_version'],
         info['journal_mode'],
     )])

    cfg = config.option.liveconfig
    if cfg:
        if "?" in cfg:
            url, token = cfg.split("?", 1)
            summary.append('Liveconfig provider: {}?<token ommitted>'.format(url))
        else:
            summary.append('Liveconfig file: {}'.format(cfg))
    return summary


class SessionLiveConfigFromFile:
    def __init__(self, fn):
        self.fn = fn
        self.configlist = []
        for line in open(fn):
            if line.strip() and not line.strip().startswith('#'):
                d = {}
                for part in line.split():
                    name, value = part.split("=")
                    d[name] = value
                self.configlist.append(d)

    def get(self, index):
        return self.configlist[index]

    def exists(self):
        return bool(self.configlist)


class SessionLiveConfigFromURL:
    def __init__(self, url):
        self.configlist = []
        self.url = url

    def get(self, index):
        try:
            return self.configlist[index]
        except IndexError:
            assert index == len(self.configlist), index
            res = requests.post(self.url)
            if res.status_code != 200:
                pytest.skip("creating newtmpuser failed {!r}".format(res))
            d = res.json()
            config = dict(addr=d["email"], mail_pw=d["password"])
            self.configlist.append(config)
            return config

    def exists(self):
        return bool(self.configlist)


@pytest.fixture(scope="session")
def session_liveconfig(request):
    liveconfig_opt = request.config.option.liveconfig
    if liveconfig_opt:
        if liveconfig_opt.startswith("http"):
            return SessionLiveConfigFromURL(liveconfig_opt)
        else:
            return SessionLiveConfigFromFile(liveconfig_opt)


@pytest.fixture
def data(request):
    class Data:
        def __init__(self):
            # trying to find test data heuristically
            # because we are run from a dev-setup with pytest direct,
            # through tox, and then maybe also from deltachat-binding
            # users like "deltabot".
            self.paths = [os.path.normpath(x) for x in [
                os.path.join(os.path.dirname(request.fspath.strpath), "data"),
                os.path.join(os.path.dirname(__file__), "..", "..", "..", "test-data")
            ]]

        def get_path(self, bn):
            """ return path of file or None if it doesn't exist. """
            for path in self.paths:
                fn = os.path.join(path, *bn.split("/"))
                if os.path.exists(fn):
                    return fn
            print("WARNING: path does not exist: {!r}".format(fn))

        def read_path(self, bn, mode="r"):
            fn = self.get_path(bn)
            if fn is not None:
                with open(fn, mode) as f:
                    return f.read()

    return Data()


@pytest.fixture
def acfactory(pytestconfig, tmpdir, request, session_liveconfig, data):

    class AccountMaker:
        def __init__(self):
            self.live_count = 0
            self.offline_count = 0
            self._finalizers = []
            self._accounts = []
            self.init_time = time.time()
            self._generated_keys = ["alice", "bob", "charlie",
                                    "dom", "elena", "fiona"]
            self.set_logging_default(False)

        def finalize(self):
            while self._finalizers:
                fin = self._finalizers.pop()
                fin()

            while self._accounts:
                acc = self._accounts.pop()
                acc.shutdown()
                acc.disable_logging()

        def new_imap_conn(self, account, config_folder=None):
            imap_conn = ImapConn(account)
            self._finalizers.append(imap_conn.shutdown)
            if config_folder is not None:
                imap_conn.select_config_folder(config_folder)
            return imap_conn

        def make_account(self, path, logid, quiet=False):
            ac = Account(path, logging=self._logging, logid=logid)
            ac._evtracker = ac.add_account_plugin(FFIEventTracker(ac))
            ac.addr = ac.get_self_contact().addr
            if not quiet:
                ac.add_account_plugin(FFIEventLogger(ac, logid=logid))
            self._accounts.append(ac)
            return ac

        def set_logging_default(self, logging):
            self._logging = bool(logging)

        def get_unconfigured_account(self):
            self.offline_count += 1
            tmpdb = tmpdir.join("offlinedb%d" % self.offline_count)
            ac = self.make_account(tmpdb.strpath, logid="ac{}".format(self.offline_count))
            ac._evtracker.init_time = self.init_time
            ac._evtracker.set_timeout(2)
            return ac

        def _preconfigure_key(self, account, addr):
            # Only set a key if we haven't used it yet for another account.
            if self._generated_keys:
                keyname = self._generated_keys.pop(0)
                fname_pub = data.read_path("key/{name}-public.asc".format(name=keyname))
                fname_sec = data.read_path("key/{name}-secret.asc".format(name=keyname))
                if fname_pub and fname_sec:
                    account._preconfigure_keypair(addr, fname_pub, fname_sec)
                    return True
                else:
                    print("WARN: could not use preconfigured keys for {!r}".format(addr))

        def get_configured_offline_account(self):
            ac = self.get_unconfigured_account()

            # do a pseudo-configured account
            addr = "addr{}@offline.org".format(self.offline_count)
            ac.set_config("addr", addr)
            self._preconfigure_key(ac, addr)
            lib.dc_set_config(ac._dc_context, b"configured_addr", addr.encode("ascii"))
            ac.set_config("mail_pw", "123")
            lib.dc_set_config(ac._dc_context, b"configured_mail_pw", b"123")
            lib.dc_set_config(ac._dc_context, b"configured", b"1")
            return ac

        def get_online_config(self, pre_generated_key=True, quiet=False):
            if not session_liveconfig:
                pytest.skip("specify DCC_NEW_TMP_EMAIL or --liveconfig")
            configdict = session_liveconfig.get(self.live_count)
            self.live_count += 1
            if "e2ee_enabled" not in configdict:
                configdict["e2ee_enabled"] = "1"

            # Enable strict certificate checks for online accounts
            configdict["imap_certificate_checks"] = str(const.DC_CERTCK_STRICT)
            configdict["smtp_certificate_checks"] = str(const.DC_CERTCK_STRICT)

            tmpdb = tmpdir.join("livedb%d" % self.live_count)
            ac = self.make_account(tmpdb.strpath, logid="ac{}".format(self.live_count), quiet=quiet)
            if pre_generated_key:
                self._preconfigure_key(ac, configdict['addr'])
            ac._evtracker.init_time = self.init_time
            ac._evtracker.set_timeout(30)
            return ac, dict(configdict)

        def get_online_configuring_account(self, mvbox=False, sentbox=False, move=False,
                                           pre_generated_key=True, quiet=False, config={}):
            ac, configdict = self.get_online_config(
                pre_generated_key=pre_generated_key, quiet=quiet)
            configdict.update(config)
            configdict["mvbox_watch"] = str(int(mvbox))
            configdict["mvbox_move"] = str(int(move))
            configdict["sentbox_watch"] = str(int(sentbox))
            ac.update_config(configdict)
            ac.configure()
            return ac

        def get_one_online_account(self, pre_generated_key=True, mvbox=False, move=False):
            ac1 = self.get_online_configuring_account(
                pre_generated_key=pre_generated_key, mvbox=mvbox, move=move)
            ac1.wait_configure_finish()
            ac1.start_io()
            return ac1

        def get_two_online_accounts(self, move=False, quiet=False):
            ac1 = self.get_online_configuring_account(move=True, quiet=quiet)
            ac2 = self.get_online_configuring_account(quiet=quiet)
            ac1.wait_configure_finish()
            ac1.start_io()
            ac2.wait_configure_finish()
            ac2.start_io()
            return ac1, ac2

        def get_many_online_accounts(self, num, move=True, quiet=True):
            accounts = [self.get_online_configuring_account(move=move, quiet=quiet)
                        for i in range(num)]
            for acc in accounts:
                acc._configtracker.wait_finish()
                acc.start_io()
            return accounts

        def clone_online_account(self, account, pre_generated_key=True):
            self.live_count += 1
            tmpdb = tmpdir.join("livedb%d" % self.live_count)
            ac = self.make_account(tmpdb.strpath, logid="ac{}".format(self.live_count))
            if pre_generated_key:
                self._preconfigure_key(ac, account.get_config("addr"))
            ac._evtracker.init_time = self.init_time
            ac._evtracker.set_timeout(30)
            ac.update_config(dict(
                addr=account.get_config("addr"),
                mail_pw=account.get_config("mail_pw"),
                mvbox_watch=account.get_config("mvbox_watch"),
                mvbox_move=account.get_config("mvbox_move"),
                sentbox_watch=account.get_config("sentbox_watch"),
            ))
            ac.configure()
            return ac

        def run_bot_process(self, module, ffi=True):
            fn = module.__file__

            bot_ac, bot_cfg = self.get_online_config()

            args = [
                sys.executable,
                "-u",
                fn,
                "--email", bot_cfg["addr"],
                "--password", bot_cfg["mail_pw"],
                bot_ac.db_path,
            ]
            if ffi:
                args.insert(-1, "--show-ffi")
            print("$", " ".join(args))
            popen = subprocess.Popen(
                args=args,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,  # combine stdout/stderr in one stream
                bufsize=0,                 # line buffering
                close_fds=True,            # close all FDs other than 0/1/2
                universal_newlines=True    # give back text
            )
            bot = BotProcess(popen, bot_cfg)
            self._finalizers.append(bot.kill)
            return bot

    am = AccountMaker()
    request.addfinalizer(am.finalize)
    yield am
    if request.node.rep_call.failed:
        for ac in am._accounts:
            direct_imap.print_imap_structure_ac(ac, tmpdir)


class BotProcess:
    def __init__(self, popen, bot_cfg):
        self.popen = popen
        self.addr = bot_cfg["addr"]

        # we read stdout as quickly as we can in a thread and make
        # the (unicode) lines available for readers through a queue.
        self.stdout_queue = queue.Queue()
        self.stdout_thread = t = threading.Thread(target=self._run_stdout_thread, name="bot-stdout-thread")
        t.setDaemon(1)
        t.start()

    def _run_stdout_thread(self):
        try:
            while 1:
                line = self.popen.stdout.readline()
                if not line:
                    break
                line = line.strip()
                self.stdout_queue.put(line)
                print("bot-stdout: ", line)
        finally:
            self.stdout_queue.put(None)

    def kill(self):
        self.popen.kill()

    def wait(self, timeout=30):
        self.popen.wait(timeout=timeout)

    def fnmatch_lines(self, pattern_lines):
        patterns = [x.strip() for x in Source(pattern_lines.rstrip()).lines if x.strip()]
        for next_pattern in patterns:
            print("+++FNMATCH:", next_pattern)
            ignored = []
            while 1:
                line = self.stdout_queue.get(timeout=15)
                if line is None:
                    if ignored:
                        print("BOT stdout terminated after these lines")
                        for line in ignored:
                            print(line)
                    raise IOError("BOT stdout-thread terminated")
                if fnmatch.fnmatch(line, next_pattern):
                    print("+++MATCHED:", line)
                    break
                else:
                    print("+++IGN:", line)
                    ignored.append(line)


@pytest.fixture
def tmp_db_path(tmpdir):
    return tmpdir.join("test.db").strpath


@pytest.fixture
def lp():
    class Printer:
        def sec(self, msg):
            print()
            print("=" * 10, msg, "=" * 10)

        def step(self, msg):
            print("-" * 5, "step " + msg, "-" * 5)
    return Printer()


@pytest.hookimpl(tryfirst=True, hookwrapper=True)
def pytest_runtest_makereport(item, call):
    # execute all other hooks to obtain the report object
    outcome = yield
    rep = outcome.get_result()

    # set a report attribute for each phase of a call, which can
    # be "setup", "call", "teardown"

    setattr(item, "rep_" + rep.when, rep)
