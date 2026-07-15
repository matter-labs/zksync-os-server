use crate::AppState;
use axum::{Json, extract::State};
use serde::Serialize;
use zksync_os_backpressure::{AdjacentSnapshot, ComponentId, compute_adjacent_snapshots};
use zksync_os_observability::ComponentState;

#[derive(Serialize)]
pub(crate) struct ComponentView {
    name: ComponentId,
    #[serde(flatten)]
    state: ComponentState,
    #[serde(skip_serializing_if = "Option::is_none")]
    lag: Option<AdjacentSnapshot>,
}

pub(crate) async fn pipeline(State(state): State<AppState>) -> Json<Vec<ComponentView>> {
    let snapshot = state.pipeline_snapshot.borrow().clone();
    let mut adjacent = compute_adjacent_snapshots(&snapshot);
    Json(
        snapshot
            .into_iter()
            .map(|(id, state)| ComponentView {
                name: id,
                state,
                lag: adjacent.remove(&id),
            })
            .collect(),
    )
}
