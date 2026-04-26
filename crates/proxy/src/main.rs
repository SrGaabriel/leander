use std::{env, process::Stdio};

use tokio::{
    io::{self, copy},
    process::Command,
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let lsp_arg = args
        .windows(2)
        .find(|w| w[0] == "--lsp")
        .and_then(|w| w.get(1))
        .expect("Usage: proxy --lsp <path_to_lsp>");
    eprintln!("LSP path: {lsp_arg}");
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let mut child = Command::new(lsp_arg)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to launch LSP server");

    let mut child_stdin = child.stdin.take().expect("Failed to open stdin");
    let mut child_stdout = child.stdout.take().expect("Failed to open stdout");

    let stdin_to_child = copy(&mut stdin, &mut child_stdin);
    let child_to_stdout = copy(&mut child_stdout, &mut stdout);

    tokio::select! {
        r = stdin_to_child => { r?; }
        r = child_to_stdout => { r?; }
        status = child.wait() => {
            let status = status?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    drop(child_stdin);
    let _ = child.wait().await;
    Ok(())
}
