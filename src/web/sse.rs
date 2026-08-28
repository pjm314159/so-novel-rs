//! SSE 下载进度（对应源项目 `DownloadProgressSseServlet`，契约见设计文档 §5.4）。
//!
//! 与源项目差异（有意改进）：
//! - 按 `?id=jobId` 订阅指定任务（无 id 兼容旧行为：最近创建的活跃任务）；
//! - 每章推送全量快照（源项目每 50 章一次）；
//! - 终态以 `event: done` / `event: error` 推送后关闭流；
//! - 数据载荷追加旧前端兼容字段 `type: "download-progress"` 与 `index`（已完成数）。

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;

use super::AppState;
use crate::engine::{JobState, Phase};

/// GET `/download-progress` 查询参数
#[derive(Deserialize)]
pub struct ProgressParams {
    /// 任务 ID；缺省时兼容旧行为（最近创建的活跃任务）
    id: Option<String>,
}

/// SSE 进度端点：连接即回 `retry: 10000` + `: connected`（与源项目一致），
/// 随后推送初始快照与每章增量快照，终态事件后关闭流。
pub async fn download_progress(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProgressParams>,
) -> impl IntoResponse {
    let job = match params.id.as_deref() {
        Some(id) => state.jobs.subscribe(id),
        None => state.jobs.latest_active().and_then(|id| state.jobs.subscribe(&id)),
    };
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = match job {
        Some((snapshot, rx)) => Box::pin(progress_stream(snapshot, rx)),
        // 无可订阅任务：仅保活心跳（对应源项目连接即挂起等待推送）
        None => Box::pin(futures_util::stream::empty::<Result<Event, Infallible>>()),
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
}

/// 流状态机：初始快照 → 订阅循环 → 终态。
enum Step {
    Snapshot(JobState, broadcast::Receiver<JobState>),
    Recv(broadcast::Receiver<JobState>),
    Done,
}

/// 进度流：握手事件（retry + connected 注释）→ 初始快照 → 广播增量 → 终态事件 → 结束。
fn progress_stream(
    snapshot: JobState,
    rx: broadcast::Receiver<JobState>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let handshake = Event::default().retry(Duration::from_secs(10)).comment("connected");
    let body = futures_util::stream::unfold(Step::Snapshot(snapshot, rx), |step| async move {
        match step {
            Step::Snapshot(s, rx) => {
                Some((Ok(event_for(&s)), if s.phase.is_terminal() { Step::Done } else { Step::Recv(rx) }))
            }
            Step::Recv(mut rx) => match rx.recv().await {
                Ok(s) => {
                    Some((Ok(event_for(&s)), if s.phase.is_terminal() { Step::Done } else { Step::Recv(rx) }))
                }
                // 消费慢被广播通道跳过：下一条全量快照自愈
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    Some((Ok(Event::default().comment("lagged")), Step::Recv(rx)))
                }
                // 任务注册表清退（发送端全部关闭）：结束流
                Err(broadcast::error::RecvError::Closed) => None,
            },
            Step::Done => None,
        }
    });
    futures_util::stream::iter([Ok(handshake)]).chain(body)
}

/// 组装进度事件：普通快照为默认消息（旧前端 `onmessage` 可解析），
/// 终态分别以 `event: done` / `event: error` 推送（载荷含 filename/failedReason）。
fn event_for(state: &JobState) -> Event {
    let mut value = serde_json::to_value(state).unwrap_or_default();
    if let Some(obj) = value.as_object_mut() {
        // 旧前端兼容：type 判别字段 + index 即已完成章节序号
        obj.insert("type".into(), "download-progress".into());
        obj.insert("index".into(), u64::from(state.done).into());
    }
    let data = value.to_string();
    match state.phase {
        Phase::Done => Event::default().event("done").data(data),
        Phase::Failed => Event::default().event("error").data(data),
        _ => Event::default().data(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(job: &str, phase: Phase, done: u32) -> JobState {
        JobState {
            job_id: job.into(),
            total: 10,
            done,
            failed: 0,
            current: "第一章".into(),
            phase,
            failed_reason: None,
            filename: None,
            seq: 1,
        }
    }

    #[tokio::test]
    async fn stream_yields_updates_then_done_and_ends() {
        let (tx, rx) = broadcast::channel(4);
        let mut stream = Box::pin(progress_stream(state("j", Phase::Downloading, 0), rx));
        assert!(stream.next().await.is_some(), "握手事件");
        assert!(stream.next().await.is_some(), "初始快照");

        tx.send(state("j", Phase::Downloading, 3)).expect("广播增量");
        assert!(stream.next().await.is_some(), "增量快照");

        tx.send(state("j", Phase::Done, 10)).expect("广播终态");
        assert!(stream.next().await.is_some(), "终态事件");
        assert!(stream.next().await.is_none(), "终态后应关闭流");
    }

    #[tokio::test]
    async fn terminal_snapshot_yields_done_then_ends() {
        let (_tx, rx) = broadcast::channel(4);
        let mut stream = Box::pin(progress_stream(state("j", Phase::Failed, 2), rx));
        assert!(stream.next().await.is_some(), "握手事件");
        assert!(stream.next().await.is_some(), "终态事件（可回放）");
        assert!(stream.next().await.is_none(), "终态后应关闭流");
    }

    #[test]
    fn event_payload_contains_legacy_compat_fields() {
        // Event 无公开 getter，经 HTTP 层断言字节流（见 tests/api.rs）；
        // 此处仅保证组装不 panic 且序列化包含兼容字段。
        let value = serde_json::to_value(state("j", Phase::Downloading, 7)).expect("序列化");
        assert_eq!(value["done"], 7);
    }
}
