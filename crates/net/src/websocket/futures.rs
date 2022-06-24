//! The wrapper around `WebSocket` API using the Futures API to be used in async rust
//!
//! # Example
//!
//! ```rust
//! use gloo_net::websocket::{Message, futures::WebSocket};
//! use wasm_bindgen_futures::spawn_local;
//! use futures::{SinkExt, StreamExt};
//!
//! # macro_rules! console_log {
//! #    ($($expr:expr),*) => {{}};
//! # }
//! # fn no_run() {
//! let mut ws = WebSocket::open("wss://echo.websocket.org").unwrap();
//! let (mut write, mut read) = ws.split();
//!
//! spawn_local(async move {
//!     write.send(Message::Text(String::from("test"))).await.unwrap();
//!     write.send(Message::Text(String::from("test 2"))).await.unwrap();
//! });
//!
//! spawn_local(async move {
//!     while let Some(msg) = read.next().await {
//!         console_log!(format!("1. {:?}", msg))
//!     }
//!     console_log!("WebSocket Closed")
//! })
//! # }
//! ```
use crate::js_to_js_error;
use crate::websocket::{events::CloseEvent, Message, State, WebSocketError};
use futures_channel::mpsc;
use futures_core::{ready, Stream};
use futures_sink::Sink;
use gloo_utils::errors::JsError;
use pin_project::{pin_project, pinned_drop};
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent};

/// Wrapper around browser's WebSocket API.
#[allow(missing_debug_implementations)]
#[pin_project(PinnedDrop)]
pub struct WebSocket {
    ws: web_sys::WebSocket,
    sink_waker: Rc<RefCell<Option<Waker>>>,
    #[pin]
    message_receiver: mpsc::UnboundedReceiver<StreamMessage>,
    #[allow(clippy::type_complexity)]
    closures: Rc<(
        Closure<dyn FnMut()>,
        Closure<dyn FnMut(MessageEvent)>,
        Closure<dyn FnMut(web_sys::Event)>,
        Closure<dyn FnMut(web_sys::CloseEvent)>,
    )>,
}

impl WebSocket {
    /// Establish a WebSocket connection.
    ///
    /// This function may error in the following cases:
    /// - The port to which the connection is being attempted is being blocked.
    /// - The URL is invalid.
    ///
    /// The error returned is [`JsError`]. See the
    /// [MDN Documentation](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/WebSocket#exceptions_thrown)
    /// to learn more.
    pub fn open(url: &str) -> Result<Self, JsError> {
        Self::setup(web_sys::WebSocket::new(url))
    }

    /// Establish a WebSocket connection.
    ///
    /// This function may error in the following cases:
    /// - The port to which the connection is being attempted is being blocked.
    /// - The URL is invalid.
    /// - The specified protocol is not supported
    ///
    /// The error returned is [`JsError`]. See the
    /// [MDN Documentation](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/WebSocket#exceptions_thrown)
    /// to learn more.
    pub fn open_with_protocol(url: &str, protocol: &str) -> Result<Self, JsError> {
        Self::setup(web_sys::WebSocket::new_with_str(url, protocol))
    }

    /// Establish a WebSocket connection.
    ///
    /// This function may error in the following cases:
    /// - The port to which the connection is being attempted is being blocked.
    /// - The URL is invalid.
    /// - The specified protocols are not supported
    /// - The protocols cannot be converted to a JSON string list
    ///
    /// The error returned is [`JsError`]. See the
    /// [MDN Documentation](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/WebSocket#exceptions_thrown)
    /// to learn more.
    pub fn open_with_protocols<S: AsRef<str> + serde::Serialize>(
        url: &str,
        protocols: &[S],
    ) -> Result<Self, JsError> {
        Self::setup(web_sys::WebSocket::new_with_str_sequence(
            url,
            &JsValue::from_serde(protocols).map_err(|err| {
                js_sys::Error::new(&format!(
                    "Failed to convert protocols to Javascript value: {}",
                    err
                ))
            })?,
        ))
    }

    fn setup(ws: Result<web_sys::WebSocket, JsValue>) -> Result<Self, JsError> {
        let waker: Rc<RefCell<Option<Waker>>> = Rc::new(RefCell::new(None));
        let ws = ws.map_err(js_to_js_error)?;

        // We rely on this because the other type Blob can be converted to Vec<u8> only through a
        // promise which makes it awkward to use in our event callbacks where we want to guarantee
        // the order of the events stays the same.
        ws.set_binary_type(BinaryType::Arraybuffer);

        let (sender, receiver) = mpsc::unbounded();

        let open_callback: Closure<dyn FnMut()> = {
            let waker = Rc::clone(&waker);
            Closure::wrap(Box::new(move || {
                if let Some(waker) = waker.borrow_mut().take() {
                    waker.wake();
                }
            }) as Box<dyn FnMut()>)
        };

        ws.set_onopen(Some(open_callback.as_ref().unchecked_ref()));

        let message_callback: Closure<dyn FnMut(MessageEvent)> = {
            let sender = sender.clone();
            Closure::wrap(Box::new(move |e: MessageEvent| {
                let sender = sender.clone();
                let msg = parse_message(e);
                let _ = sender.unbounded_send(StreamMessage::Message(msg));
            }) as Box<dyn FnMut(MessageEvent)>)
        };

        ws.set_onmessage(Some(message_callback.as_ref().unchecked_ref()));

        let error_callback: Closure<dyn FnMut(web_sys::Event)> = {
            let sender = sender.clone();
            Closure::wrap(Box::new(move |_e: web_sys::Event| {
                let sender = sender.clone();
                let _ = sender.unbounded_send(StreamMessage::ErrorEvent);
            }) as Box<dyn FnMut(web_sys::Event)>)
        };

        ws.set_onerror(Some(error_callback.as_ref().unchecked_ref()));

        let close_callback: Closure<dyn FnMut(web_sys::CloseEvent)> = {
            Closure::wrap(Box::new(move |e: web_sys::CloseEvent| {
                let sender = sender.clone();
                let close_event = CloseEvent {
                    code: e.code(),
                    reason: e.reason(),
                    was_clean: e.was_clean(),
                };
                let _ = sender.unbounded_send(StreamMessage::CloseEvent(close_event));
                let _ = sender.unbounded_send(StreamMessage::ConnectionClose);
            }) as Box<dyn FnMut(web_sys::CloseEvent)>)
        };

        ws.set_onclose(Some(close_callback.as_ref().unchecked_ref()));

        Ok(Self {
            ws,
            sink_waker: waker,
            message_receiver: receiver,
            closures: Rc::new((
                open_callback,
                message_callback,
                error_callback,
                close_callback,
            )),
        })
    }

    /// Closes the websocket.
    ///
    /// See the [MDN Documentation](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/close#parameters)
    /// to learn about parameters passed to this function and when it can return an `Err(_)`
    pub fn close(self, code: Option<u16>, reason: Option<&str>) -> Result<(), JsError> {
        let result = match (code, reason) {
            (None, None) => self.ws.close(),
            (Some(code), None) => self.ws.close_with_code(code),
            (Some(code), Some(reason)) => self.ws.close_with_code_and_reason(code, reason),
            // default code is 1005 so we use it,
            // see: https://developer.mozilla.org/en-US/docs/Web/API/WebSocket/close#parameters
            (None, Some(reason)) => self.ws.close_with_code_and_reason(1005, reason),
        };
        result.map_err(js_to_js_error)
    }

    /// The current state of the websocket.
    pub fn state(&self) -> State {
        let ready_state = self.ws.ready_state();
        match ready_state {
            0 => State::Connecting,
            1 => State::Open,
            2 => State::Closing,
            3 => State::Closed,
            _ => unreachable!(),
        }
    }

    /// The extensions in use.
    pub fn extensions(&self) -> String {
        self.ws.extensions()
    }

    /// The sub-protocol in use.
    pub fn protocol(&self) -> String {
        self.ws.protocol()
    }
}

#[derive(Clone)]
enum StreamMessage {
    ErrorEvent,
    CloseEvent(CloseEvent),
    Message(Message),
    ConnectionClose,
}

fn parse_message(event: MessageEvent) -> Message {
    if let Ok(array_buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
        let array = js_sys::Uint8Array::new(&array_buffer);
        Message::Bytes(array.to_vec())
    } else if let Ok(txt) = event.data().dyn_into::<js_sys::JsString>() {
        Message::Text(String::from(&txt))
    } else {
        unreachable!("message event, received Unknown: {:?}", event.data());
    }
}

impl Sink<Message> for WebSocket {
    type Error = WebSocketError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready_state = self.ws.ready_state();
        if ready_state == 0 {
            *self.sink_waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let result = match item {
            Message::Bytes(bytes) => self.ws.send_with_u8_array(&bytes),
            Message::Text(message) => self.ws.send_with_str(&message),
        };
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(WebSocketError::MessageSendError(js_to_js_error(e))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let msg = ready!(self.project().message_receiver.poll_next(cx));
        match msg {
            Some(StreamMessage::Message(msg)) => Poll::Ready(Some(Ok(msg))),
            Some(StreamMessage::ErrorEvent) => {
                Poll::Ready(Some(Err(WebSocketError::ConnectionError)))
            }
            Some(StreamMessage::CloseEvent(e)) => {
                Poll::Ready(Some(Err(WebSocketError::ConnectionClose(e))))
            }
            Some(StreamMessage::ConnectionClose) => Poll::Ready(None),
            None => Poll::Ready(None),
        }
    }
}

#[pinned_drop]
impl PinnedDrop for WebSocket {
    fn drop(self: Pin<&mut Self>) {
        self.ws.close().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use wasm_bindgen_futures::spawn_local;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    const ECHO_SERVER_URL: &str = env!("ECHO_SERVER_URL");

    #[wasm_bindgen_test]
    fn websocket_works() {
        let ws = WebSocket::open(ECHO_SERVER_URL).unwrap();
        let (mut sender, mut receiver) = ws.split();

        spawn_local(async move {
            sender
                .send(Message::Text(String::from("test 1")))
                .await
                .unwrap();
            sender
                .send(Message::Text(String::from("test 2")))
                .await
                .unwrap();
        });

        spawn_local(async move {
            // ignore first message
            // the echo-server used sends it's info in the first message
            // let _ = ws.next().await;

            assert_eq!(
                receiver.next().await.unwrap().unwrap(),
                Message::Text("test 1".to_string())
            );
            assert_eq!(
                receiver.next().await.unwrap().unwrap(),
                Message::Text("test 2".to_string())
            );
        });
    }
}