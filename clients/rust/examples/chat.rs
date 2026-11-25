//! `GATEWAY_URL=https://gateway.example.com GATEWAY_API_KEY=gw_... cargo run --example chat -- chat-default "Say hello."`
//!
//! Sends one chat call and one streamed call and prints the answers.

use futures_util::StreamExt;
use qucelerate_ai_gateway::{ChatRequest, Client, Message, StreamEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("GATEWAY_URL")?;
    let key = std::env::var("GATEWAY_API_KEY")?;
    let mut args = std::env::args().skip(1);
    let model = args.next().unwrap_or_else(|| "chat-default".into());
    let prompt = args.next().unwrap_or_else(|| "Say hello.".into());
    let client = Client::builder(url).api_key(key).build()?;
    let request = ChatRequest::new(model, vec![Message::user(prompt)]).max_tokens(256);

    let response = client.chat(&request).await?;
    println!("chat: {}", response.content().unwrap_or_default());
    if let Some(usage) = &response.usage {
        println!("usage: {usage:?}");
    }

    let mut stream = client.chat_stream(&request).await?;
    print!("stream: ");
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::ChoiceDelta(delta) => {
                print!("{}", delta.content_delta.unwrap_or_default())
            }
            StreamEvent::ResponseCompleted(done) => println!("\nusage: {:?}", done.usage),
            _ => {}
        }
    }
    Ok(())
}
