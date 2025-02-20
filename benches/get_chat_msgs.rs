use async_std::path::Path;

use criterion::async_executor::AsyncStdExecutor;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

use deltachat::chat::{self, ChatId};
use deltachat::chatlist::Chatlist;
use deltachat::context::Context;

async fn get_chat_msgs_benchmark(dbfile: &Path, chats: &[ChatId]) {
    let id = 100;
    let context = Context::new(dbfile.into(), id).await.unwrap();

    for c in chats.iter().take(10) {
        black_box(chat::get_chat_msgs(&context, *c, 0).await.ok());
    }
}

fn criterion_benchmark(c: &mut Criterion) {
    // To enable this benchmark, set `DELTACHAT_BENCHMARK_DATABASE` to some large database with many
    // messages, such as your primary account.
    if let Ok(path) = std::env::var("DELTACHAT_BENCHMARK_DATABASE") {
        let chats: Vec<_> = async_std::task::block_on(async {
            let context = Context::new((&path).into(), 100).await.unwrap();
            let chatlist = Chatlist::try_load(&context, 0, None, None).await.unwrap();
            let len = chatlist.len();
            (0..len).map(|i| chatlist.get_chat_id(i).unwrap()).collect()
        });

        c.bench_function("chat::get_chat_msgs (load messages from 10 chats)", |b| {
            b.to_async(AsyncStdExecutor)
                .iter(|| get_chat_msgs_benchmark(black_box(path.as_ref()), black_box(&chats)))
        });
    } else {
        println!("env var not set: DELTACHAT_BENCHMARK_DATABASE");
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
