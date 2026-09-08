//! Offline HTTP fixture shared by transport, orchestrator and interface tests.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

pub(crate) struct MockServer {
    pub(crate) url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    pub(crate) fn new(handler: impl Fn(&Value) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        listener.set_nonblocking(true).expect("nonblocking");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("blocking client socket");
                        let handler = handler.clone();
                        let captured = captured.clone();
                        workers.push(thread::spawn(move || {
                            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
                            stream.set_write_timeout(Some(Duration::from_secs(5))).expect("write timeout");
                            let request = read_request(&mut stream);
                            captured.lock().expect("requests lock").push(request.clone());
                            let (status, body) = handler(&request);
                            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            // A cancellation test deliberately closes the client side early.
                            let _ = stream.write_all(response.as_bytes());
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
            for worker in workers {
                worker.join().expect("mock worker");
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub(crate) fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("mock server");
            }
        }
    }
}

pub(crate) fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).expect("read request");
        assert_ne!(read, 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            assert!(headers.starts_with("POST /chat/completions HTTP/1.1"));
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("length"))
                })
                .expect("content-length");
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                    .expect("request JSON");
            }
        }
    }
}

pub(crate) fn text_response(content: &str) -> (u16, String) {
    (
        200,
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":content},"finish_reason":"stop"}]}),
            json!({"choices":[], "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
        ),
    )
}

pub(crate) fn tool_response(calls: Vec<Value>) -> (u16, String) {
    (
        200,
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
            })
        ),
    )
}

pub(crate) fn delegate(index: usize, id: &str, handle: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{"name":"delegate_task","arguments":json!({"handle":handle,"task":"Проверь ответ"}).to_string()}})
}
