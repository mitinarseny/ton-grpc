use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::Service;
use anyhow::{anyhow, Result};
use futures::{FutureExt, try_join};
use tokio::sync::watch::Receiver;
use tokio::time::{interval, MissedTickBehavior};
use tokio_retry::Retry;
use tokio_retry::strategy::{ExponentialBackoff, jitter};
use tower::limit::ConcurrencyLimit;
use tower::load::peak_ewma::Cost;
use tracing::{instrument, trace};
use crate::block::{BlockIdExt, BlocksGetShards, Sync};
use crate::block::{BlockHeader, BlockId, BlocksLookupBlock, BlocksGetBlockHeader, GetMasterchainInfo, MasterchainInfo};
use crate::session::{SessionClient, SessionRequest};
use crate::request::{Callable, Request, RequestBody, TypedCallable};

#[derive(Clone)]
pub struct CursorClient {
    client: ConcurrencyLimit<SessionClient>,

    pub first_block_rx: Receiver<Option<(BlockHeader, BlockHeader)>>,
    pub last_block_rx: Receiver<Option<(BlockHeader, BlockHeader)>>,

    masterchain_info_rx: Receiver<Option<MasterchainInfo>>
}

impl CursorClient {
    pub fn new(client: ConcurrencyLimit<SessionClient>) -> Self {
        let (ctx, crx) = tokio::sync::watch::channel(None);
        let (mtx, mrx) = tokio::sync::watch::channel(None);
        tokio::spawn({
            let mut client = client.clone();
            async move {
                let mut timer = interval(Duration::new(2, 1_000_000_000 / 2));
                timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

                let mut current: Option<MasterchainInfo> = None;
                loop {
                    timer.tick().await;

                    let masterchain_info = GetMasterchainInfo::default()
                        .typed_call(&mut client)
                        .await;

                    match masterchain_info {
                        Ok(mut masterchain_info) => {
                            if let Some(cur) = current.clone() {
                                if cur == masterchain_info {
                                    trace!(cursor = cur.last.seqno, "block actual");

                                    continue;
                                } else {
                                    trace!(cursor = cur.last.seqno, actual = masterchain_info.last.seqno, "block discovered")
                                }
                            }

                            match fetch_last_headers(&mut client).await {
                                Ok((last_master_chain_header, last_work_chain_header)) => {
                                    masterchain_info.last = last_master_chain_header.id.clone();
                                    trace!(seqno = last_master_chain_header.id.seqno, "master chain block reached");
                                    trace!(seqno = last_work_chain_header.id.seqno, "work chain block reached");

                                    current.replace(masterchain_info.clone());

                                    let _ = mtx.send(Some(masterchain_info));
                                    let _ = ctx.send(Some((last_master_chain_header, last_work_chain_header)));
                                },
                                Err(e) => trace!(e = ?e, "unable to fetch last headers")
                            }
                        },
                        Err(e) => trace!(e = ?e, "unable to get master chain info")
                    }
                }
            }
        });

        let (ftx, frx) = tokio::sync::watch::channel(None);
        tokio::spawn({
            let mut client = client.clone();
            let mut first_block: Option<(BlockHeader, BlockHeader)> = None;

            async move {
                let mut timer = interval(Duration::from_secs(30));
                timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
                loop {
                    timer.tick().await;

                    if let Some((mfb, wfb)) = first_block.clone() {
                        let mut clone = client.clone();
                        if let Err(e) = try_join!(
                            BlocksGetShards::new(mfb.id.clone()).typed_call(&mut clone),
                            BlocksGetBlockHeader::new(wfb.id.clone()).typed_call(&mut client)
                        ) {
                            trace!(seqno = mfb.id.seqno, e = ?e, "first block not available anymore");
                            first_block = None;
                        } else {
                            trace!("first block still available");
                        }
                    }
                    if first_block.is_none() {
                        let fb = find_first_blocks(&mut client).await;

                        match fb {
                            Ok((mfb, wfb)) => {
                                trace!(seqno = mfb.id.seqno, "master chain first block");
                                trace!(seqno = wfb.id.seqno, "work chain first block");

                                first_block = Some((mfb.clone(), wfb.clone()));

                                let _ = ftx.send(Some((mfb, wfb)));
                            },
                            Err(e) => trace!(e = ?e, "unable to fetch first headers")
                        }
                    }
                }
            }
        });

        Self {
            client,

            first_block_rx: frx,
            last_block_rx: crx,
            masterchain_info_rx: mrx
        }
    }

    pub fn headers(&self, chain_id: i32) -> Option<(BlockHeader, BlockHeader)> {
        let Some(first_block) = self.first_block_rx.borrow().clone() else {
            return None;
        };
        let Some(last_block) = self.last_block_rx.borrow().clone() else {
            return None;
        };

        match chain_id {
            -1 => Some((first_block.0, last_block.0)),
            _ => Some((first_block.1, last_block.1))
        }
    }
}

impl<R : TypedCallable<ConcurrencyLimit<SessionClient>>> Service<R> for CursorClient {
    type Response = R::Response;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        if self.last_block_rx.borrow().is_some()
            && self.first_block_rx.borrow().is_some()
            && self.masterchain_info_rx.borrow().is_some() {
            return Service::<SessionRequest>::poll_ready(&mut self.client, cx)
        }

        cx.waker().wake_by_ref();

        Poll::Pending
    }

    fn call(&mut self, req: R) -> Self::Future {
        let mut client = self.client.clone();

        async move {
            req.typed_call(&mut client).await
        }.boxed()
    }
}

impl Service<SessionRequest> for CursorClient {
    type Response = <SessionClient as Service<SessionRequest>>::Response;
    type Error = <SessionClient as Service<SessionRequest>>::Error;
    type Future = <SessionClient as Service<SessionRequest>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.last_block_rx.borrow().is_some()
            && self.first_block_rx.borrow().is_some()
            && self.masterchain_info_rx.borrow().is_some() {
            return Service::<SessionRequest>::poll_ready(&mut self.client, cx)
        }

        cx.waker().wake_by_ref();

        Poll::Pending
    }

    fn call(&mut self, req: SessionRequest) -> Self::Future {
        match req {
            SessionRequest::Atomic(Request { body: RequestBody::GetMasterchainInfo(_), .. }) => {
                let masterchain_info = self.masterchain_info_rx.borrow().as_ref().unwrap().clone();
                async { Ok(serde_json::to_value(masterchain_info)?) }.boxed()
            },
            _ => self.client.call(req).boxed()
        }
    }
}

impl tower::load::Load for CursorClient {
    type Metric = Cost;

    fn load(&self) -> Self::Metric {
        self.client.load()
    }
}

async fn check_block_available(client: &mut ConcurrencyLimit<SessionClient>, block_id: BlockId) -> Result<(BlockHeader, BlockHeader)> {
    let block_id = BlocksLookupBlock::seqno(block_id).typed_call(client).await?;
    let shards = BlocksGetShards::new(block_id.clone()).typed_call(client).await?;

    let mut clone = client.clone();
    try_join!(
        BlocksGetBlockHeader::new(block_id).typed_call(&mut clone),
        BlocksGetBlockHeader::new(shards.shards.first().expect("must be exist").clone()).typed_call(client)
    )
}

#[instrument(skip_all, err, level = "trace")]
async fn find_first_blocks(client: &mut ConcurrencyLimit<SessionClient>) -> Result<(BlockHeader, BlockHeader)> {
    let start = GetMasterchainInfo::default()
        .typed_call(client)
        .await?.last;

    let length = start.seqno;
    let mut rhs = length;
    let mut lhs = 1;
    let mut cur = (lhs + rhs) / 2;

    let workchain = start.workchain;
    let shard = start.shard;

    let mut block = check_block_available(client, BlockId::new(workchain, shard, cur)).await;

    while lhs < rhs {
        // TODO[akostylev0] specify error
        if block.is_err() {
            lhs = cur + 1;
        } else {
            rhs = cur;
        }

        cur = (lhs + rhs) / 2;

        if cur == 0 {
            break;
        }

        trace!("lhs: {}, rhs: {}, cur: {}", lhs, rhs, cur);

        block = check_block_available(client, BlockId::new(workchain, shard, cur)).await;
    }

    let (master, work) = block?;

    trace!(seqno = master.id.seqno, "first seqno");

    Ok((master, work))
}


async fn fetch_last_headers(client: &mut ConcurrencyLimit<SessionClient>) -> Result<(BlockHeader, BlockHeader)> {
    let master_chain_last_block_id = Sync::default()
        .typed_call(client)
        .await?;

    let shards = BlocksGetShards::new(master_chain_last_block_id.clone())
        .typed_call(client)
        .await?.shards;

    let work_chain_last_block_id = shards.first()
        .ok_or_else(|| anyhow!("last block for work chain not found"))?
        .clone();

    let mut clone = client.clone();
    let (master_chain_header, work_chain_header) = try_join!(
        BlocksGetBlockHeader::new(master_chain_last_block_id).typed_call(&mut clone),
        wait_for_block_header(work_chain_last_block_id, client)
    )?;

    Ok((master_chain_header, work_chain_header))
}

async fn wait_for_block_header(block_id: BlockIdExt, client: &mut ConcurrencyLimit<SessionClient>) -> Result<BlockHeader> {
    let retry = ExponentialBackoff::from_millis(4)
        .max_delay(Duration::from_secs(1))
        .map(jitter)
        .take(16);

    Retry::spawn(retry, || {
        let block_id = block_id.clone();
        let mut client = client.clone();

        async move {
            BlocksGetBlockHeader::new(block_id).typed_call(&mut client).await
        }
    }).await
}
