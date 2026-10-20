//! The raw line protocol over TCP, so `nc localhost 8081` just works.

use std::io;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

use crate::server::{MAX_LINE, State, run_session};

pub async fn serve(listener: TcpListener, state: Arc<State>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let (read, write) = stream.into_split();
            // A line over the limit, or invalid UTF-8, ends the stream and so
            // the session: there is no telling where the next request starts.
            let incoming = FramedRead::new(read, LinesCodec::new_with_max_length(MAX_LINE))
                .take_while(|line| std::future::ready(line.is_ok()))
                .filter_map(|line| std::future::ready(line.ok()));
            let outgoing = FramedWrite::new(write, LinesCodec::new());
            run_session(incoming, outgoing, state).await;
        });
    }
}
