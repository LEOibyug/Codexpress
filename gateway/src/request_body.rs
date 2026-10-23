//! Request ingestion has no application byte ceiling. Upstreams own payload
//! limits; local transport/allocation failures must not masquerade as HTTP 413.
use axum::{body::Body, http::StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;

pub(crate) struct ReadError {
    pub status: StatusCode,
    pub message: &'static str,
}

pub(crate) async fn read(body: Body, ingress: &'static str) -> Result<Bytes, ReadError> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            eprintln!(
                "request_body_read_failure unix={} ingress={} received_bytes={} reason=transport",
                crate::model::now(),
                ingress,
                bytes.len()
            );
            ReadError {
                status: StatusCode::BAD_REQUEST,
                message: "请求正文读取中断，请重新发送",
            }
        })?;
        bytes.try_reserve(chunk.len()).map_err(|_| {
            eprintln!(
                "request_body_read_failure unix={} ingress={} received_bytes={} reason=allocation",
                crate::model::now(),
                ingress,
                bytes.len()
            );
            ReadError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "本机内存不足，暂时无法接收请求",
            }
        })?;
        bytes.extend_from_slice(&chunk);
    }
    eprintln!(
        "request_body_received unix={} ingress={} bytes={}",
        crate::model::now(),
        ingress,
        bytes.len()
    );
    Ok(Bytes::from(bytes))
}
