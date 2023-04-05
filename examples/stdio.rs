use std::time::Duration;

use async_lsp::stdio::{PipeStdin, PipeStdout};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    eprintln!("Non blocking");
    {
        let mut stdin = PipeStdin::lock()?;
        let mut stdout = PipeStdout::lock()?;
        let mut buf = [0u8; 4 << 10];
        let mut i = 0u64;
        loop {
            match timeout(Duration::from_secs(1), stdin.read(&mut buf)).await {
                Ok(ret) => {
                    let len = ret?;
                    stdout.write_all(&buf[..len]).await?;
                    if buf[..len].contains(&b'x') {
                        break;
                    }
                }
                Err(_) => {
                    i += 1;
                    stdout.write_all(i.to_string().as_bytes()).await?;
                }
            }
        }
    }

    eprintln!("Blocking");
    {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        println!("{buf:?}");
    }

    Ok(())
}
