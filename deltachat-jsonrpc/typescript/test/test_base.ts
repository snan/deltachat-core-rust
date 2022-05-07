import { tmpdir } from "os";
import { join } from "path";
import { mkdtemp, rm } from "fs/promises";
import { existsSync } from "fs";
import { spawn, exec } from "child_process";
import { unwrapPromise } from "./ts_helpers.js";
import fetch from "node-fetch";
/* port is not configurable yet */


function getTargetDir(): Promise<string> {
  return new Promise((res, rej) => {
    exec(
      "cargo metadata --no-deps --format-version 1",
      (error, stdout, stderr) => {
        if (error) {
          console.log("error", error);
          rej(error);
        } else {
          try {
            const json = JSON.parse(stdout);
            res(json.target_directory);
          } catch (error) {
            console.log("json error", error);
            rej(error);
          }
        }
      }
    );
  });
}

export const CMD_API_SERVER_PORT = 20808;
export async function startCMD_API_Server(port: typeof CMD_API_SERVER_PORT) {
  const tmp_dir = await mkdtemp(join(tmpdir(), "test_prefix"));

  const path_of_server = join(await getTargetDir(), "debug/webserver");
  console.log(path_of_server);

  if (!existsSync(path_of_server)) {
    throw new Error(
      "server executable does not exist, you need to build it first" +
        "\nserver executable not found at " +
        path_of_server
    );
  }

  const server = spawn(path_of_server, {
    cwd: tmp_dir,
    env: {
      RUST_LOG: "info",
    },
  });
  let should_close = false;

  server.on("exit", () => {
    if (should_close) {
      return;
    }
    throw new Error("Server quit");
  });

  server.stderr.pipe(process.stderr);

  //server.stdout.pipe(process.stdout)

  return {
    close: async () => {
      should_close = true;
      if (!server.kill(9)) {
        console.log("server termination failed");
      }
      await rm(tmp_dir, { recursive: true });
    },
  };
}

export type CMD_API_Server_Handle = unwrapPromise<
  ReturnType<typeof startCMD_API_Server>
>;

export async function createTempUser(url: string) {
  async function postData(url = "") {
    // Default options are marked with *
    const response = await fetch(url, {
      method: "POST", // *GET, POST, PUT, DELETE, etc.
      headers: {
        "cache-control": "no-cache",
      },
    });
    return response.json(); // parses JSON response into native JavaScript objects
  }

  return await postData(url);
}
