use futures::{stream, Stream};
use prost::Message;
use std::pin::Pin;
use tonic::Status;

pub(super) type ListStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

// Leave room for clients whose default protobuf receive limit is four MiB.
const BATCH_BYTES: usize = 4 * 1024 * 1024 - 5;

pub(super) fn batches<T, R>(items: Vec<T>, response: fn(Vec<T>) -> R) -> ListStream<R>
where
    T: Message + Send + 'static,
    R: Send + 'static,
{
    batches_with_limit(items, response, BATCH_BYTES)
}

fn batches_with_limit<T, R>(items: Vec<T>, response: fn(Vec<T>) -> R, limit: usize) -> ListStream<R>
where
    T: Message + Send + 'static,
    R: Send + 'static,
{
    let mut items = items
        .into_iter()
        .map(|item| {
            let len = item.encoded_len();
            // Every stream reply uses one repeated message at field number 1.
            (item, 1 + prost::length_delimiter_len(len) + len)
        })
        .peekable();
    Box::pin(stream::iter(std::iter::from_fn(move || {
        let mut batch = Vec::new();
        let mut bytes = 0;
        while let Some((_, item_bytes)) = items.peek() {
            if *item_bytes > limit - bytes {
                break;
            }
            bytes += *item_bytes;
            batch.push(items.next().expect("peeked item").0);
        }
        if !batch.is_empty() {
            return Some(Ok(response(batch)));
        }
        if items.next().is_some() {
            // The client must discard a partial stream when it sees an error.
            items.by_ref().for_each(drop);
            return Some(Err(Status::resource_exhausted(
                "a CRI list item exceeds the gRPC receive limit",
            )));
        }
        None
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::runtime::v1::{Image, StreamImagesResponse};
    use futures::StreamExt;

    fn response(images: Vec<Image>) -> StreamImagesResponse {
        StreamImagesResponse { images }
    }

    #[tokio::test]
    async fn batches_preserve_items_and_end_without_empty_replies() {
        let images: Vec<_> = (0..7)
            .map(|id| Image {
                id: id.to_string(),
                ..Default::default()
            })
            .collect();
        let mut stream = batches_with_limit(images.clone(), response, 10);
        let mut seen = Vec::new();
        let mut sizes = Vec::new();
        while let Some(reply) = stream.next().await {
            let reply = reply.unwrap();
            assert!(reply.encoded_len() <= 10);
            sizes.push(reply.images.len());
            seen.extend(reply.images);
        }
        assert_eq!(sizes, [2, 2, 2, 1]);
        assert_eq!(seen, images);
        assert!(batches(Vec::new(), response).next().await.is_none());
    }

    #[tokio::test]
    async fn oversized_items_terminate_the_stream() {
        let images = vec![
            Image {
                id: "matching immutable image".to_string(),
                ..Default::default()
            },
            Image::default(),
        ];
        let mut stream = batches_with_limit(images, response, 20);
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        assert!(stream.next().await.is_none());
    }
}
