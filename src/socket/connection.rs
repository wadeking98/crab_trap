use std::io::{stdin, Write};
use std::sync::Arc;

use crate::listener;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Config, Editor};
use termion::clear;
use termion::event::{Event, Key};
use termion::input::TermReadEventsAndRaw;
use termion::raw::RawTerminal;
use tokio::select;
use tokio::sync::broadcast::{self, Sender as HandleSender};
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::watch::{self, Receiver};
use tokio::sync::Mutex;
use tokio::task;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct Handle {
    pub rl: Arc<Mutex<Editor<(), FileHistory>>>,
    pub tx: HandleSender<&'static str>,
    pub soc_kill_token: CancellationToken,
    pub raw_mode: bool,
}

async fn handle_key_input() -> Option<(Key, Vec<u8>)> {
    let (tx, mut rx) = mpsc::channel(1024);
    // stdin().keys() blocks the main thread so we have to spawn a new one and run it there
    task::spawn(async move {
        let key_input = stdin().events_and_raw().next();
        tx.send(key_input).await.unwrap();
    });
    let key_res = rx.recv().await.unwrap();
    return match key_res {
        Some(key) => {
            return match key {
                Ok((Event::Key(k), raw)) => Some((k, raw)),
                Err(_) => None,
                _ => None,
            };
        }
        None => None,
    };
}

pub async fn read_line(
    rl: Arc<Mutex<Editor<(), FileHistory>>>,
    prompt: Option<&str>,
) -> Result<String, ReadlineError> {
    let (tx, mut rx) = mpsc::channel::<Result<String, ReadlineError>>(1024);
    let input_prompt = match prompt {
        Some(val) => String::from(val),
        None => String::from(""),
    };
    task::spawn(async move {
        let mut reader = rl.lock().await;

        let raw_content = reader.readline(&input_prompt);

        let content = match raw_content {
            Ok(line) => {
                reader.add_history_entry(line.clone()).unwrap_or_default();
                Ok(line + "\n")
            }
            Err(e) => Err(e),
        };
        tx.send(content).await.unwrap_or_default();
    });
    let received_content = rx.recv().await.unwrap()?;
    return Ok(received_content);
}

impl Handle {
    pub fn new() -> (Handle, CancellationToken) {
        let (tx, _) = broadcast::channel::<&str>(1024);
        let soc_kill_token = CancellationToken::new();
        let soc_kill_token_listen = soc_kill_token.clone();
        let mut builder = Config::builder();
        builder = builder.check_cursor_position(false);
        let config = builder.build();
        let rl = Arc::new(Mutex::new(
            Editor::<(), FileHistory>::with_config(config).unwrap(),
        ));
        let handle = Handle {
            rl,
            tx,
            soc_kill_token,
            raw_mode: false,
        };
        return (handle, soc_kill_token_listen);
    }

    pub fn handle_listen<W>(
        &self,
        handle_to_soc_send: Sender<String>,
        mut soc_to_handle_recv: Receiver<String>,
        mut stdout: RawTerminal<W>,
    ) where
        W: Write + Send + 'static,
    {
        let tx = self.tx.clone();
        let rl = self.rl.clone();
        let tx_copy = self.tx.clone();
        let mut raw_mode = self.raw_mode;
        let (prompt_tx, mut prompt_rx) = watch::channel(String::from(""));
        let (raw_mode_tx, mut raw_mode_rx) = mpsc::channel::<bool>(1024);
        // start reader
        tokio::spawn(async move {
            let mut active = false;

            loop {
                if !active {
                    if listener::wait_for_signal(tx_copy.subscribe(), "start", Some(&mut raw_mode))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    active = true;
                }
                // wait for new read content or pause notification
                select! {
                    _ = soc_to_handle_recv.changed() =>{
                        let resp = soc_to_handle_recv.borrow().to_owned();
                        let outp =match raw_mode{
                            true =>resp,
                            false => format!("{clear}\r{resp}", clear = clear::CurrentLine)
                        };
                        stdout.write_all(outp.as_bytes()).unwrap();
                        stdout.flush().unwrap();
                        let new_prompt = match outp.split("\n").last(){
                            Some(s)=>s,
                            None => ""
                        };
                        if prompt_tx.send(String::from(new_prompt)).err().is_some() {
                            continue;
                        }
                    }
                    _ = listener::wait_for_signal(tx_copy.subscribe(), "quit", Some(&mut raw_mode)) =>{
                        stdout.suspend_raw_mode().unwrap();
                        active = false;
                    }
                    raw_term_state = raw_mode_rx.recv() =>{
                        if raw_term_state.is_none(){
                            println!("Terminal closed");
                            continue;
                        }
                        match raw_term_state.unwrap(){
                            true => stdout.activate_raw_mode().unwrap_or_default(),
                            false => stdout.suspend_raw_mode().unwrap_or_default()
                        };
                    }
                }
            }
        });
        // start writer
        tokio::spawn(async move {
            // wait for start signal
            if listener::wait_for_signal(tx.subscribe(), "start", Some(&mut raw_mode))
                .await
                .is_err()
            {
                return;
            }
            loop {
                if !raw_mode {
                    raw_mode_tx.send(false).await.unwrap();
                    let new_prompt = prompt_rx.borrow_and_update().to_owned();
                    let mut content = match read_line(rl.clone(), Some(new_prompt.as_str())).await {
                        Ok(val) => val,
                        Err(_) => continue,
                    };

                    if content.trim_end().eq("back") {
                        println!("{clear}", clear = clear::BeforeCursor);
                        //notify the reader that we're pausing
                        tx.send("quit").unwrap();
                        // send a new line so we get a prompt when we return
                        content = String::from("\n");
                        if listener::wait_for_signal(tx.subscribe(), "start", Some(&mut raw_mode))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    if handle_to_soc_send.send(content).await.is_err() {
                        return;
                    }
                } else {
                    raw_mode_tx.send(true).await.unwrap();
                    let input_opt = handle_key_input().await;
                    if input_opt.is_none() {
                        continue;
                    }

                    let (key_val, key_bytes) = input_opt.unwrap();
                    if key_val == Key::Ctrl('b') {
                        println!("{clear}", clear = clear::BeforeCursor);
                        tx.send("quit").unwrap();
                        raw_mode_tx.send(false).await.unwrap();
                        if listener::wait_for_signal(tx.subscribe(), "start", Some(&mut raw_mode))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if !raw_mode {
                            continue;
                        }
                        raw_mode_tx.send(true).await.unwrap();
                        handle_to_soc_send.send(String::from("\n")).await.unwrap()
                    }
                    handle_to_soc_send
                        .send(String::from_utf8_lossy(&key_bytes).into_owned())
                        .await
                        .unwrap();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use termion::raw::IntoRawMode;
    use tokio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
    };

    use super::*;

    #[tokio::test]
    async fn test_handle() {
        let listener_res = TcpListener::bind("127.0.0.1:32426").await;
        assert!(listener_res.is_ok());
        let listener = listener_res.unwrap();
        tokio::spawn(async move {
            let (mut tcp_stream, _) = listener.accept().await.unwrap();
            //mock return vale from soc
            tcp_stream.write("mock value".as_bytes()).await.unwrap();
        });
        let stream = TcpStream::connect("127.0.0.1:32426").await.unwrap();
        let (handle, cancel_token) = Handle::new();
        let (handle_to_soc_send, handle_to_soc_recv) = mpsc::channel::<String>(1024);
        let (soc_to_handle_send, soc_to_handle_recv) = watch::channel::<String>(String::from(""));
        let out = std::io::Cursor::new(Vec::new()).into_raw_mode().unwrap();
        listener::start_socket(stream, soc_to_handle_send, handle_to_soc_recv, cancel_token);
        handle.handle_listen(handle_to_soc_send.clone(), soc_to_handle_recv.clone(), out);
        let mut rx = handle.tx.subscribe();

        //test handle channel send/receive
        tokio::spawn(async move {
            assert_eq!(rx.recv().await.unwrap(), "start");
        });
        handle.tx.send("start").unwrap();

        soc_to_handle_recv.clone().changed().await.unwrap();
        assert_eq!("mock value", soc_to_handle_recv.borrow().as_str());
    }
}
