mod framing;
mod goals;
mod lsp;
mod snoop;
mod state;

use std::{env, process::Stdio};

use tokio::{io, process::Command, sync::watch};

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let lsp_arg = args
        .windows(2)
        .find(|w| w[0] == "--lsp")
        .and_then(|w| w.get(1))
        .expect("Usage: proxy --lsp <path_to_lsp>");
    eprintln!("[proxy] launching: {lsp_arg} serve");

    let mut child = Command::new(lsp_arg)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to launch LSP server");

    let child_stdin = child.stdin.take().expect("child stdin");
    let child_stdout = child.stdout.take().expect("child stdout");

    let state = state::StateHandle::new();
    let (cursor_tx, cursor_rx) = watch::channel(None);

    let (lsp_handle, c2s_done) = lsp::spawn(child_stdin, child_stdout, cursor_tx, state.clone());
    goals::spawn(lsp_handle, cursor_rx, state);

    tokio::select! {
        _ = c2s_done => {
            eprintln!("[proxy] zed closed stdin");
        }
        status = child.wait() => {
            let status = status?;
            eprintln!("[proxy] lake exited: {status}");
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    let _ = child.wait().await;
    Ok(())
}
