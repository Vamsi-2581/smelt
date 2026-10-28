//! 工作区目录的单条命令：改库，改了就把整份目录推给订阅了 `workspace.changed` 的客户端。
//! 关掉一段对话时顺带收掉它的终端和 ACP 会话：对话归运行时，关它的人只需要发这一条。
//!
//! 库由调用方给：控制分发传默认主库，测试传临时库。

use std::sync::Arc;

use smelt_core::automation_store::AutomationStore;
use smelt_core::session_control::is_background_acp_session_id;
use smelt_core::workspace_catalog::{self, DirectoryChange, DirectoryError};
use smelt_core::workspace_directory::{
    WorkspaceConversation, WorkspaceDirectory, WorkspaceRevision,
};
use smelt_runtime_client::protocol::control::{ControlError, ControlErrorCode};
use smelt_store::Store;

use super::{AcpSessions, EventHubHandle, RemoteSessions, Sessions, dlog};

/// 读当前的工作区目录。新建会话校验项目、删会话认类型、唤醒挂起的对话、Pi 通讯录都按它
/// 判断，不另存一份。生产从主库现读；测试换成固定的一份（测试进程里默认主库是关着的）。
#[derive(Clone)]
pub(crate) struct WorkspaceDirectoryReader(
    Arc<dyn Fn() -> Result<WorkspaceDirectory, String> + Send + Sync>,
);

impl WorkspaceDirectoryReader {
    pub(crate) fn from_store() -> Self {
        Self(Arc::new(|| {
            workspace_catalog::directory().map_err(|error| error.to_string())
        }))
    }

    /// 守护还没起好（没有服务状态）时用：读不到目录。
    pub(crate) fn unavailable() -> Self {
        Self(Arc::new(|| Err("工作区目录还没就绪".to_string())))
    }

    #[cfg(test)]
    pub(crate) fn fixed(directory: WorkspaceDirectory) -> Self {
        Self(Arc::new(move || Ok(directory.clone())))
    }

    pub(crate) fn read(&self) -> Result<WorkspaceDirectory, String> {
        (self.0)()
    }

    /// 目录里有没有以 `sid` 为 ACP 会话的对话。读不到目录按没有算。
    pub(crate) fn has_acp(&self, sid: &str) -> bool {
        self.read().is_ok_and(|directory| {
            directory
                .conversations
                .iter()
                .any(|conversation| conversation.acp.as_ref().is_some_and(|acp| acp.sid == sid))
        })
    }
}

/// 守护的默认主库。
pub(crate) fn default_store() -> Result<Store, ControlError> {
    smelt_core::sqlite_state::default_sqlite_store()
        .map_err(|message| ControlError::new(ControlErrorCode::OperationFailed, message))
}

pub(crate) fn create_conversation(
    store: &Store,
    conversation: &WorkspaceConversation,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change =
        workspace_catalog::create_conversation_on(store, conversation).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

pub(crate) fn update_conversation(
    store: &Store,
    conversation: &WorkspaceConversation,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change =
        workspace_catalog::update_conversation_on(store, conversation).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

/// 先收会话再删目录：收不掉（provider 没能退出）就报错、目录留着，用户还能再关一次。
/// 自动化 Run 的会话归运行时管，这里只把它从目录里拿掉，不收。
#[allow(clippy::too_many_arguments)]
pub(crate) fn delete_conversation(
    store: &Store,
    id: &str,
    sessions: &Sessions,
    acp_sessions: &AcpSessions,
    remote_sessions: &RemoteSessions,
    automations: &AutomationStore,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let id = id.trim();
    let directory = workspace_catalog::directory_on(store).map_err(control_error)?;
    let Some(conversation) = directory
        .conversations
        .iter()
        .find(|conversation| conversation.id == id)
    else {
        return Ok(WorkspaceRevision {
            revision: directory.revision,
        });
    };
    for terminal in conversation.terminals.iter().filter_map(|t| t.id.as_deref()) {
        super::kill_terminal_session(terminal, sessions, remote_sessions, event_hub)
            .map_err(|message| ControlError::new(ControlErrorCode::OperationFailed, message))?;
    }
    if let Some(acp) = &conversation.acp
        && !is_background_acp_session_id(&acp.sid)
        && !automations.lock().unwrap().is_run_session(&acp.sid)
    {
        super::kill_acp_session(&acp.sid, acp_sessions, remote_sessions, event_hub)
            .map_err(|message| ControlError::new(ControlErrorCode::OperationFailed, message))?;
    }
    let change = workspace_catalog::delete_conversation_on(store, id).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

pub(crate) fn open_project(
    store: &Store,
    root: &str,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change = workspace_catalog::open_project_on(store, root).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

pub(crate) fn close_project(
    store: &Store,
    root: &str,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change = workspace_catalog::close_project_on(store, root).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

/// `session.delete` 删掉的会话从目录里摘掉，改了就推（见 [`workspace_catalog::forget_session_on`]）。
/// 进程调用方已经收掉了，这里只改目录。
pub(crate) fn forget_session(
    store: &Store,
    session_id: &str,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change = workspace_catalog::forget_session_on(store, session_id).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

/// 远程改名之后，把以 `sid` 为 ACP 会话的那段对话也改名，改了就推。
pub(crate) fn rename_acp_conversation(
    store: &Store,
    sid: &str,
    title: &str,
    event_hub: &EventHubHandle,
) -> Result<WorkspaceRevision, ControlError> {
    let change =
        workspace_catalog::rename_acp_conversation_on(store, sid, title).map_err(control_error)?;
    Ok(publish(&change, event_hub))
}

/// 存储层直接改了目录（守护确认或放弃一段对话的恢复身份）之后，把当前目录推出去。
pub(crate) fn publish_current(event_hub: &EventHubHandle) {
    match workspace_catalog::directory() {
        Ok(directory) => {
            if let Err(error) = event_hub.publish_workspace(&directory) {
                dlog(&format!("推送工作区目录失败：{error}"));
            }
        }
        Err(error) => dlog(&format!("读取工作区目录失败：{error}")),
    }
}

/// 改动已经落库，推送失败不算命令失败：订阅方重连时首帧会带上当前目录。
fn publish(change: &DirectoryChange, event_hub: &EventHubHandle) -> WorkspaceRevision {
    if change.changed
        && let Err(error) = event_hub.publish_workspace(&change.directory)
    {
        dlog(&format!("推送工作区目录失败：{error}"));
    }
    WorkspaceRevision {
        revision: change.directory.revision,
    }
}

fn control_error(error: DirectoryError) -> ControlError {
    let code = match &error {
        DirectoryError::NotFound(_) => ControlErrorCode::NotFound,
        DirectoryError::Invalid(_) => ControlErrorCode::InvalidParams,
        DirectoryError::Store(_) => ControlErrorCode::OperationFailed,
    };
    ControlError::new(code, error.to_string())
}
