use crate::{
    FirestoreDb, FirestoreError, FirestorePartition, FirestorePartitionQueryParams,
    FirestoreQueryCursor, FirestoreQueryParams, FirestoreResult,
};
use async_trait::async_trait;
use chrono::prelude::*;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::FutureExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::{future, StreamExt};
use gcloud_sdk::google::firestore::v1::*;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::*;

pub type PeekableBoxStream<'a, T> = futures::stream::Peekable<BoxStream<'a, T>>;

#[async_trait]
pub trait FirestoreQuerySupport {
    async fn query_doc(&self, params: FirestoreQueryParams) -> FirestoreResult<Vec<Document>>;

    async fn stream_query_doc<'b>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, Document>>;

    async fn stream_query_doc_with_errors<'b>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, FirestoreResult<Document>>>;

    async fn query_obj<T>(&self, params: FirestoreQueryParams) -> FirestoreResult<Vec<T>>
    where
        for<'de> T: Deserialize<'de>;
    async fn stream_query_obj<'b, T>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, T>>
    where
        for<'de> T: Deserialize<'de>;

    async fn stream_query_obj_with_errors<'b, T>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, FirestoreResult<T>>>
    where
        for<'de> T: Deserialize<'de>,
        T: Send + 'b;

    fn stream_partition_cursors_with_errors(
        &self,
        params: FirestorePartitionQueryParams,
    ) -> BoxFuture<FirestoreResult<PeekableBoxStream<FirestoreResult<FirestoreQueryCursor>>>>;

    async fn stream_partition_query_doc_with_errors(
        &self,
        parallelism: usize,
        partition_params: FirestorePartitionQueryParams,
    ) -> FirestoreResult<BoxStream<FirestoreResult<(FirestorePartition, Document)>>>;

    async fn stream_partition_query_obj_with_errors<'a, T>(
        &'a self,
        parallelism: usize,
        partition_params: FirestorePartitionQueryParams,
    ) -> FirestoreResult<BoxStream<'a, FirestoreResult<(FirestorePartition, T)>>>
    where
        for<'de> T: Deserialize<'de>,
        T: Send + 'a;
}

impl FirestoreDb {
    fn create_query_request(
        &self,
        params: &FirestoreQueryParams,
    ) -> FirestoreResult<tonic::Request<RunQueryRequest>> {
        Ok(tonic::Request::new(RunQueryRequest {
            parent: params
                .parent
                .as_ref()
                .unwrap_or_else(|| self.get_documents_path())
                .clone(),
            consistency_selector: self
                .session_params
                .consistency_selector
                .as_ref()
                .map(|selector| selector.try_into())
                .transpose()?,
            query_type: Some(run_query_request::QueryType::StructuredQuery(params.into())),
        }))
    }

    fn stream_query_doc_with_retries<'a, 'b>(
        &'a self,
        params: FirestoreQueryParams,
        retries: usize,
        span: &'a Span,
    ) -> BoxFuture<'a, FirestoreResult<BoxStream<'b, FirestoreResult<Option<Document>>>>> {
        async move {
            let query_request = self.create_query_request(&params)?;
            let begin_query_utc: DateTime<Utc> = Utc::now();

            match self
                .client
                .get()
                .run_query(query_request)
                .map_err(|e| e.into())
                .await
            {
                Ok(query_response) => {
                    let query_stream = query_response
                        .into_inner()
                        .map_ok(|r| r.document)
                        .map_err(|e| e.into())
                        .boxed();

                    let end_query_utc: DateTime<Utc> = Utc::now();
                    let query_duration = end_query_utc.signed_duration_since(begin_query_utc);

                    span.record(
                        "/firestore/response_time",
                        query_duration.num_milliseconds(),
                    );
                    span.in_scope(|| {
                        debug!(
                            "[DB]: Querying stream of documents in {:?} took {}ms",
                            params.collection_id,
                            query_duration.num_milliseconds()
                        );
                    });

                    Ok(query_stream)
                }
                Err(err) => match err {
                    FirestoreError::DatabaseError(ref db_err)
                        if db_err.retry_possible && retries < self.options.max_retries =>
                    {
                        warn!(
                            "[DB]: Failed with {}. Retrying: {}/{}",
                            db_err,
                            retries + 1,
                            self.options.max_retries
                        );

                        self.stream_query_doc_with_retries(params, retries + 1, span)
                            .await
                    }
                    _ => Err(err),
                },
            }
        }
        .boxed()
    }

    fn query_doc_with_retries<'a>(
        &'a self,
        params: FirestoreQueryParams,
        retries: usize,
        span: &'a Span,
    ) -> BoxFuture<'a, FirestoreResult<Vec<Document>>> {
        async move {
            let query_request = self.create_query_request(&params)?;
            let begin_query_utc: DateTime<Utc> = Utc::now();

            match self
                .client
                .get()
                .run_query(query_request)
                .map_err(|e| e.into())
                .await
            {
                Ok(query_response) => {
                    let query_stream = query_response
                        .into_inner()
                        .map_ok(|rs| rs.document)
                        .try_collect::<Vec<Option<Document>>>()
                        .await?
                        .into_iter()
                        .flatten()
                        .collect();
                    let end_query_utc: DateTime<Utc> = Utc::now();
                    let query_duration = end_query_utc.signed_duration_since(begin_query_utc);

                    span.record(
                        "/firestore/response_time",
                        query_duration.num_milliseconds(),
                    );
                    span.in_scope(|| {
                        debug!(
                            "[DB]: Querying documents in {:?} took {}ms",
                            params.collection_id,
                            query_duration.num_milliseconds()
                        );
                    });

                    Ok(query_stream)
                }
                Err(err) => match err {
                    FirestoreError::DatabaseError(ref db_err)
                        if db_err.retry_possible && retries < self.options.max_retries =>
                    {
                        warn!(
                            "[DB]: Failed with {}. Retrying: {}/{}",
                            db_err,
                            retries + 1,
                            self.options.max_retries
                        );
                        self.query_doc_with_retries(params, retries + 1, span).await
                    }
                    _ => Err(err),
                },
            }
        }
        .boxed()
    }
}

#[async_trait]
impl FirestoreQuerySupport for FirestoreDb {
    async fn query_doc(&self, params: FirestoreQueryParams) -> FirestoreResult<Vec<Document>> {
        let collection_str = params.collection_id.to_string();
        let span = span!(
            Level::DEBUG,
            "Firestore Query",
            "/firestore/collection_name" = collection_str.as_str(),
            "/firestore/response_time" = field::Empty
        );
        self.query_doc_with_retries(params, 0, &span).await
    }

    async fn stream_query_doc<'b>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, Document>> {
        let collection_str = params.collection_id.to_string();

        let span = span!(
            Level::DEBUG,
            "Firestore Streaming Query",
            "/firestore/collection_name" = collection_str.as_str(),
            "/firestore/response_time" = field::Empty
        );

        let doc_stream = self.stream_query_doc_with_retries(params, 0, &span).await?;

        Ok(Box::pin(doc_stream.filter_map(|doc_res| {
            future::ready(match doc_res {
                Ok(Some(doc)) => Some(doc),
                Ok(None) => None,
                Err(err) => {
                    error!("[DB] Error occurred while consuming query: {}", err);
                    None
                }
            })
        })))
    }

    async fn stream_query_doc_with_errors<'b>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, FirestoreResult<Document>>> {
        let collection_str = params.collection_id.to_string();

        let span = span!(
            Level::DEBUG,
            "Firestore Streaming Query",
            "/firestore/collection_name" = collection_str.as_str(),
            "/firestore/response_time" = field::Empty
        );

        let doc_stream = self.stream_query_doc_with_retries(params, 0, &span).await?;

        Ok(Box::pin(doc_stream.filter_map(|doc_res| {
            future::ready(match doc_res {
                Ok(Some(doc)) => Some(Ok(doc)),
                Ok(None) => None,
                Err(err) => {
                    error!("[DB] Error occurred while consuming query: {}", err);
                    Some(Err(err))
                }
            })
        })))
    }

    async fn query_obj<T>(&self, params: FirestoreQueryParams) -> FirestoreResult<Vec<T>>
    where
        for<'de> T: Deserialize<'de>,
    {
        let doc_vec = self.query_doc(params).await?;
        doc_vec
            .iter()
            .map(|doc| Self::deserialize_doc_to(doc))
            .collect()
    }

    async fn stream_query_obj<'b, T>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, T>>
    where
        for<'de> T: Deserialize<'de>,
    {
        let doc_stream = self.stream_query_doc(params).await?;
        Ok(Box::pin(doc_stream.filter_map(|doc| async move {
            match Self::deserialize_doc_to::<T>(&doc) {
                Ok(obj) => Some(obj),
                Err(err) => {
                    error!(
                        "[DB] Error occurred while consuming query document as a stream: {}",
                        err
                    );
                    None
                }
            }
        })))
    }

    async fn stream_query_obj_with_errors<'b, T>(
        &self,
        params: FirestoreQueryParams,
    ) -> FirestoreResult<BoxStream<'b, FirestoreResult<T>>>
    where
        for<'de> T: Deserialize<'de>,
        T: Send + 'b,
    {
        let doc_stream = self.stream_query_doc_with_errors(params).await?;
        Ok(Box::pin(doc_stream.and_then(|doc| {
            future::ready(Self::deserialize_doc_to::<T>(&doc))
        })))
    }

    fn stream_partition_cursors_with_errors(
        &self,
        params: FirestorePartitionQueryParams,
    ) -> BoxFuture<FirestoreResult<PeekableBoxStream<FirestoreResult<FirestoreQueryCursor>>>> {
        Box::pin(async move {
            let consistency_selector: Option<
                gcloud_sdk::google::firestore::v1::partition_query_request::ConsistencySelector,
            > = self
                .session_params
                .consistency_selector
                .as_ref()
                .map(|selector| selector.try_into())
                .transpose()?;

            let stream: PeekableBoxStream<FirestoreResult<FirestoreQueryCursor>> =
                futures::stream::unfold(
                    Some((params, consistency_selector)),
                    move |maybe_params| async move {
                        if let Some((params, maybe_consistency_selector)) = maybe_params {
                            let request = tonic::Request::new(PartitionQueryRequest {
                                page_size: params.page_size as i32,
                                partition_count: params.partition_count as i64,
                                parent: params
                                    .query_params
                                    .parent
                                    .as_ref()
                                    .unwrap_or_else(|| self.get_documents_path())
                                    .clone(),
                                consistency_selector: maybe_consistency_selector.clone(),
                                query_type: Some(
                                    partition_query_request::QueryType::StructuredQuery(
                                        params.query_params.to_structured_query(),
                                    ),
                                ),
                                page_token: params.page_token.clone().unwrap_or_default(),
                            });

                            match self.client().get().partition_query(request).await {
                                Ok(response) => {
                                    let partition_response = response.into_inner();
                                    let firestore_cursors: Vec<FirestoreQueryCursor> =
                                        partition_response
                                            .partitions
                                            .into_iter()
                                            .map(|e| e.into())
                                            .collect();

                                    if !partition_response.next_page_token.is_empty() {
                                        Some((
                                            Ok(firestore_cursors),
                                            Some((
                                                params.with_page_token(
                                                    partition_response.next_page_token,
                                                ),
                                                maybe_consistency_selector,
                                            )),
                                        ))
                                    } else {
                                        Some((Ok(firestore_cursors), None))
                                    }
                                }
                                Err(err) => Some((Err(FirestoreError::from(err)), None)),
                            }
                        } else {
                            None
                        }
                    },
                )
                .flat_map(|s| {
                    futures::stream::iter(match s {
                        Ok(results) => results
                            .into_iter()
                            .map(Ok::<FirestoreQueryCursor, FirestoreError>)
                            .collect(),
                        Err(err) => vec![Err(err)],
                    })
                })
                .boxed()
                .peekable();

            Ok(stream)
        })
    }

    async fn stream_partition_query_doc_with_errors(
        &self,
        parallelism: usize,
        partition_params: FirestorePartitionQueryParams,
    ) -> FirestoreResult<BoxStream<FirestoreResult<(FirestorePartition, Document)>>> {
        let collection_str = partition_params.query_params.collection_id.to_string();

        let span = span!(
            Level::DEBUG,
            "Firestore Streaming Partition Query",
            "/firestore/collection_name" = collection_str
        );

        span.in_scope(|| {
            debug!(
                "Running query on partitions with max parallelism: {}",
                parallelism
            )
        });

        let mut cursors: Vec<FirestoreQueryCursor> = self
            .stream_partition_cursors_with_errors(partition_params.clone())
            .await?
            .try_collect()
            .await?;

        if cursors.is_empty() {
            span.in_scope(|| {
                debug!(
                    "The server detected the query has too few results to be partitioned. Falling back to normal query"
                )
            });
            let doc_stream = self
                .stream_query_doc_with_errors(partition_params.query_params)
                .await?;

            Ok(doc_stream
                .and_then(|doc| future::ready(Ok((FirestorePartition::new(), doc))))
                .boxed())
        } else {
            let mut cursors_pairs: Vec<Option<FirestoreQueryCursor>> =
                Vec::with_capacity(cursors.len() + 2);
            cursors_pairs.push(None);
            cursors_pairs.extend(cursors.drain(..).into_iter().map(Some));
            cursors_pairs.push(None);

            let (tx, rx) =
                mpsc::unbounded_channel::<FirestoreResult<(FirestorePartition, Document)>>();

            futures::stream::iter(cursors_pairs.windows(2))
                .map(|cursor_pair| (cursor_pair, tx.clone(), partition_params.clone(), span.clone()))
                .for_each_concurrent(
                    Some(parallelism),
                    |(cursor_pair, tx, partition_params, span)| async move {
                        span.in_scope(|| {
                            debug!(
                                    "Streaming partition cursor {:?}",
                                    cursor_pair
                                )
                        });

                        let mut params_with_cursors = partition_params.query_params;
                        if let Some(first_cursor) = cursor_pair.first() {
                            params_with_cursors.mopt_start_at(first_cursor.clone());
                        }
                        if let Some(last_cursor) = cursor_pair.last() {
                            params_with_cursors.mopt_end_at(last_cursor.clone());
                        }

                        let partition = FirestorePartition::new().opt_start_at(params_with_cursors.start_at.clone()).opt_end_at(params_with_cursors.end_at.clone());

                        match self.stream_query_doc_with_errors(params_with_cursors).await {
                            Ok(result_stream) => {
                                result_stream
                                    .map(|doc_res| (doc_res, tx.clone(), span.clone(), partition.clone()))
                                    .for_each(|(doc_res, tx, span, partition)| async move {

                                        let message = doc_res.map(|doc| (partition.clone(), doc));
                                        if let Err(err) = tx.send(message) {
                                            span.in_scope(|| {
                                                warn!(
                                                    "Unable to send result for partition {:?}:{:?}",
                                                    partition,
                                                    err
                                                )
                                            })
                                        };
                                    }).await;
                            },
                            Err(err) => {
                                if let Err(err) = tx.send(Err(err)) {
                                    span.in_scope(|| {
                                        warn!(
                                                "Unable to send result for partition cursor {:?} error {:?}",
                                                cursor_pair,
                                                err
                                            )
                                    })
                                };
                            }
                        }
                    },
                ).await;

            Ok(Box::pin(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            ))
        }
    }

    async fn stream_partition_query_obj_with_errors<'a, T>(
        &'a self,
        parallelism: usize,
        partition_params: FirestorePartitionQueryParams,
    ) -> FirestoreResult<BoxStream<'a, FirestoreResult<(FirestorePartition, T)>>>
    where
        for<'de> T: Deserialize<'de>,
        T: Send + 'a,
    {
        let doc_stream = self
            .stream_partition_query_doc_with_errors(parallelism, partition_params)
            .await?;

        Ok(Box::pin(doc_stream.and_then(|(partition, doc)| {
            future::ready(Self::deserialize_doc_to::<T>(&doc).map(|obj| (partition, obj)))
        })))
    }
}
