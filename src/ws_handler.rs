use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade, CloseFrame}, State},
    response::IntoResponse,
};
use axum_extra::TypedHeader;
use broadcaster::BroadcastChannel;
use serde::{Serialize, Deserialize};

use std::{ops::ControlFlow, borrow::Cow, sync::Arc, collections::HashMap};
use std::net::SocketAddr;

use futures::{lock::Mutex, channel::mpsc::SendError};

//allows to extract the IP of connecting user
use axum::extract::connect_info::ConnectInfo;

//allows to split the websocket stream into separate TX and RX branches
use futures::{sink::SinkExt, stream::StreamExt};

use crate::{WSState, UserCursor};

// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
// of websocket negotiation). After this completes, the actual switching from HTTP to
// websocket protocol will occur.
// This is the last point where we can extract TCP/IP metadata such as IP address of the client
// as well as things from HTTP headers such as user-agent of the browser etc.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,

    State(ws_state): State<WSState>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    println!("`{user_agent}` at {addr} connected.");

    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr, ws_state.sender_broadcaster, ws_state.user_cursors, ws_state.shared_counter))
}

// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(
    mut socket: WebSocket,
    who: SocketAddr,
    sender_broadcaster: BroadcastChannel<String>,
    user_cursors: Arc<Mutex<HashMap<String, UserCursor>>>,
    shared_counter: Arc<Mutex<i32>>,
){
    //send a ping (unsupported by some browsers) just to kick things off and get a response
    if socket.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
        println!("Pinged {who}...");
    } else {
        println!("Could not send ping {who}!");
        // no Error here since the only thing we can do is to close the connection.
        // If we cannot send messages, there is no way to salvage the statemachine anyway.
        return;
    }

    if let Some(msg) = socket.recv().await {
        if let Ok(msg) = msg {
            if process_message(&msg).is_break() {
                return;
            }
            println!("client {who} ponged back");
        } else {
            println!("client {who} abruptly disconnected");
            return;
        }
    }

    let initial_user_cursors_arc_clone = Arc::clone(&user_cursors);
    let initial_user_cursors_to_populate = query_user_cursors(initial_user_cursors_arc_clone).await;
    for (_, value) in initial_user_cursors_to_populate.into_iter() {
        let cursor_event_as_string = serde_json::to_string(&value).unwrap();
        if socket
            .send(Message::Text(format!("{{\"cursor_event\":{cursor_event_as_string}}}")))
            .await
            .is_err()
        {
            println!("Could not populate initial user cursors to {who}!");
            return;
        }
    }

    let initial_shared_counter_clone = Arc::clone(&shared_counter);
    let initial_counter_as_text = query_counter(initial_shared_counter_clone).await.to_string();
    if socket
        .send(Message::Text(format!("{{\"counter\":{initial_counter_as_text}}}")))
        .await
        .is_err()
    {
        println!("Could not send initial counter val to {who}!");
        return;
    }

    // By splitting socket we can send and receive at the same time. In this example we will send
    // unsolicited messages to client based on some sort of server's internal event (i.e .timer).
    let (mut sender, mut receiver) = socket.split();

    let mut broadcaster_clone_for_event_loop = sender_broadcaster.clone();

    // Spawn a task that will push several messages to the client (does not matter what client does)
    let mut send_task = tokio::spawn(async move {
        loop {
            let latest_event_option = broadcaster_clone_for_event_loop.next().await;
            let event = match latest_event_option {
                Some(v) => v,
                None => break,
            };

            // In case of any websocket error, we exit.
            if sender
                .send(Message::Text(event))
                .await
                .is_err()
            {
                break;
            }
        }

        println!("Sending close to {who}...");
        if let Err(e) = sender
            .send(Message::Close(Some(CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: Cow::from("Goodbye"),
            })))
            .await
        {
            println!("Could not send Close due to {e}, probably it is ok?");
        }
    });

    if insert_my_new_user_cursor(Arc::clone(&user_cursors), who)
        .await
        .is_err()
    {
        println!("Could not send this user's cursor val to {who}!");
        return;
    }

    let user_cursors_clone_for_receiving = Arc::clone(&user_cursors);
    let shared_counter_clone_for_receiving = Arc::clone(&shared_counter);
    let broadcaster_clone_for_receiving = sender_broadcaster.clone();

    // This second task will receive messages from client and print them on server console
    let mut recv_task = tokio::spawn(async move {
        let mut cnt = 0;
        while let Some(Ok(msg)) = receiver.next().await {
            cnt += 1;

            // print message and break if instructed to do so
            if process_message(&msg.clone()).is_break() {
                break;
            }

            let msg_as_text: String;
            match msg.clone().into_text() {
                Ok(value) => {
                    msg_as_text = value;
                }
                Err(_) => {
                    break;
                }
            }

            let client_state_update: ClientStateUpdate = serde_json::from_str(&msg_as_text).unwrap();

            match client_state_update.object_name.as_str() {
                "shared_counter" => {
                    if mutate_counter(Arc::clone(&shared_counter_clone_for_receiving), broadcaster_clone_for_receiving.clone(), client_state_update.new_state_serial)
                        .await
                        .is_err()
                    {
                        break;
                    }
                },
                "my_user_cursor" => {
                    if mutate_my_user_cursor(Arc::clone(&user_cursors_clone_for_receiving), broadcaster_clone_for_receiving.clone(), serde_json::from_str(&client_state_update.new_state_serial).unwrap(), who)
                        .await
                        .is_err()
                    {
                        break;
                    }
                },
                _=>{
                    println!("Unsupported object to update");
                },
            };
            
        }
        cnt
    });

    // If any one of the tasks exit, abort the other.
    tokio::select! {
        rv_a = (&mut send_task) => {
            match rv_a {
                Ok(()) => println!("A bunch of messages sent to {who}"),
                Err(a) => println!("Error sending messages {a:?}")
            }
            recv_task.abort();
        },
        rv_b = (&mut recv_task) => {
            match rv_b {
                Ok(b) => println!("Received {b} messages"),
                Err(b) => println!("Error receiving messages {b:?}")
            }
            send_task.abort();
        }
    }

    // returning from the handler closes the websocket connection
    println!("Websocket context {who} destroyed");

    let _ = delete_my_user_cursor(user_cursors, who).await;
}

// helper to print contents of messages to stdout. Has special treatment for Close.
fn process_message(msg: &Message) -> ControlFlow<(), ()> {
    match msg {
        Message::Close(_) => {
            return ControlFlow::Break(());
        }
        _ => {
            return ControlFlow::Continue(());
        }
    };
}

pub async fn query_user_cursors(user_cursors: Arc<Mutex<HashMap<String, UserCursor>>>) -> HashMap<String, UserCursor> {
    let user_cursors = user_cursors.lock().await;
    let user_cursor_clone = user_cursors.clone();

    user_cursor_clone
}

async fn insert_my_new_user_cursor(user_cursors: Arc<Mutex<HashMap<String, UserCursor>>>, who: SocketAddr) -> Result<(), SendError> {
    let mut user_cursors_at_this_moment = user_cursors.lock().await;
    let new_user_cursor = UserCursor::new( 0.0, 0.0);
    user_cursors_at_this_moment.insert(who.to_string(), new_user_cursor.clone());

    Ok(())
}

async fn mutate_my_user_cursor(user_cursors: Arc<Mutex<HashMap<String, UserCursor>>>, user_cursors_broadcaster: BroadcastChannel<String>, updated_cursor: UserCursor, who: SocketAddr) -> Result<(), SendError> {
    let mut user_cursors_at_this_moment = user_cursors.lock().await;

    user_cursors_at_this_moment.remove_entry(&who.to_string());
    user_cursors_at_this_moment.insert(who.to_string(), updated_cursor.clone());
    let cursor_event_as_string = serde_json::to_string(&updated_cursor).unwrap();

    user_cursors_broadcaster.send(&format!("{{\"cursor_event\":{cursor_event_as_string}}}")).await
}

async fn delete_my_user_cursor(user_cursors: Arc<Mutex<HashMap<String, UserCursor>>>, who: SocketAddr) -> Result<(), SendError> {
    let mut user_cursors_at_this_moment = user_cursors.lock().await;
    match user_cursors_at_this_moment.remove(&who.to_string()) {
        Some(_) => (),
        None => (),
    };

    Ok(())
}

pub async fn query_counter(shared_counter: Arc<Mutex<i32>>) -> i32 {
    let counter = shared_counter.lock().await;

    *counter
}

async fn mutate_counter(shared_counter: Arc<Mutex<i32>>, shared_counter_broadcaster: BroadcastChannel<String>, operation_string: String) -> Result<(), SendError> {
    let mut counter = shared_counter.lock().await;
    *counter = match operation_string.as_str() {
        "counter_plus_one" => *counter + 1,
        "counter_minus_one" => *counter - 1,
        _=>*counter,
    };
    let counter_as_string = counter.to_string();

    shared_counter_broadcaster.send(&format!("{{\"counter\":{counter_as_string}}}")).await
}

#[derive(Debug, Serialize, Deserialize)]
struct ClientStateUpdate {
    object_name: String,
    new_state_serial: String,
}