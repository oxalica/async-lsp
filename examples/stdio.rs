#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut stdin = async_lsp::stdio::PipeStdin::lock_forever().unwrap();
    let mut stdout = async_lsp::stdio::PipeStdout::lock_forever().unwrap();
    tokio::io::copy(&mut stdin, &mut stdout).await.unwrap();
}
