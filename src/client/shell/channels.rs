//! Regents channel rows of the "spaces" sidebar section. The expanded set is
//! presentation state of this client; a click on a channel asks the server to
//! open the channel as a chat pane, or to focus its chat pane.

use super::*;

impl ClientShellState {
    /// Handles a press on a channel toggle or a channel row. Returns false
    /// when `point` is on neither.
    pub(super) fn handle_channel_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let toggle = self
            .hits
            .channel_toggles
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, workspace_index)| *workspace_index);
        if let Some(workspace_index) = toggle {
            if let Some(workspace_id) = self.snapshot_workspace_id(workspace_index) {
                if !self.expanded_channel_workspaces.remove(&workspace_id) {
                    self.expanded_channel_workspaces.insert(workspace_id);
                }
                outcome.repaint = true;
            }
            return true;
        }
        let Some(hit) = self
            .hits
            .channels
            .iter()
            .find(|hit| contains(hit.rect, point))
            .copied()
        else {
            return false;
        };
        let target = self.snapshot.as_deref().and_then(|snapshot| {
            let workspace = snapshot.workspaces.get(hit.workspace_index)?;
            let channel = workspace.channels.get(hit.channel_index)?;
            Some((workspace.workspace_id.clone(), channel.clone()))
        });
        if let Some((workspace_id, channel)) = target {
            self.mode = ClientShellMode::Terminal;
            self.push_endpoint_method(
                crate::api::schema::Method::ChannelOpen(crate::api::schema::ChannelOpenParams {
                    workspace_id: Some(workspace_id),
                    channel,
                }),
                outcome,
            );
        }
        true
    }

    fn snapshot_workspace_id(&self, workspace_index: usize) -> Option<String> {
        self.snapshot
            .as_deref()?
            .workspaces
            .get(workspace_index)
            .map(|workspace| workspace.workspace_id.clone())
    }
}
