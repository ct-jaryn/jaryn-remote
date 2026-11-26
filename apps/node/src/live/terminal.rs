use std::path::Path;

use jr_session::Engine;

use crate::State;

pub(super) fn drain_shell(st: &mut State) {
    let Some(rx) = st.term_rx.as_ref() else {
        return;
    };
    let mut chunks = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        chunks.push(chunk);
    }
    if let Some(engine) = st.engine.as_mut() {
        for chunk in chunks {
            let _ = engine.write_terminal(&chunk);
        }
    }
}

pub(super) fn spawn_shell(st: &mut State, term: &str) {
    if st.term_pty.is_some() {
        return;
    }
    let mut pty = match crate::pty::spawn(term, st.term_cols, st.term_rows) {
        Ok(p) => p,
        Err(e) => {
            println!("node: terminal-failed {e}");
            return;
        }
    };
    // 读那一头单独拿一份句柄，写端留在 st 里。
    let reader = match pty.reader() {
        Ok(r) => r,
        Err(e) => {
            println!("node: terminal-reader {e}");
            None
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut reader) = reader {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    st.term_rx = Some(rx);
    st.term_pty = Some(pty);
    println!("node: terminal-spawned（pty {cols}x{rows}）", cols = st.term_cols, rows = st.term_rows);
}

pub fn note_recovery(path: &Path) {
    if Engine::recovery_pending(path) {
        println!("node: release_all");
        Engine::clear_recovery(path);
    }
}
