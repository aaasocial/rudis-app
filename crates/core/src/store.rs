//! The backend-owned state store: project + undo/redo stacks.

use crate::command::{Command, EntitySnapshot, Patch};
use crate::model::{Playback, Project};
use crate::transport::TransportCmd;
use crate::CoreError;

/// Phase 57 (plan 57-06, PLAY-07/D-08): how many times [`Store::snapshot`] has
/// deep-cloned the whole `Project` since process start.
///
/// The preview producer used to call `snapshot()` **once per produced frame**
/// — inside `resolve_multilayer`, purely to read `width`/`height`/`fps` — which
/// is the per-frame clone D-08 names by hand. This counter is the
/// observability hook that makes "the clone is gone" a MEASUREMENT rather than
/// a code review: read it around a span of production and it must not move.
///
/// Relaxed ordering: it is a diagnostic odometer, never a synchronisation
/// point, and it must not cost the IPC path anything.
pub static PROJECT_CLONE_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Single owner of the live [`Project`] plus undo/redo stacks. The HOST manages
/// exactly one `Store` behind a mutex (`app_core::SharedStore`, owned by
/// `crates/ffi`'s `FfiAppCtx`); the UI shell can only reach it through
/// commands, never directly.
#[derive(Debug, Default)]
pub struct Store {
    project: Project,
    /// Undo stack. Each entry is a GROUP of inverse commands (most recent
    /// last, LIFO). A normal `dispatch()` pushes a 1-member group; an agent
    /// turn (`begin_turn()`..`end_turn()`) pushes a single multi-member group
    /// so the whole turn reverses in ONE `undo()` call (AGENT-03).
    undo: Vec<Vec<Command>>,
    /// Redo stack, populated by `undo()`, cleared by any new `dispatch()`
    /// (standard linear-history semantics). Grouped symmetrically with `undo`.
    redo: Vec<Vec<Command>>,
    /// When `Some`, a turn is OPEN: `dispatch()` appends each inverse to this
    /// open group instead of pushing its own undo-stack entry. `None` (the
    /// `Default`) means no turn is open — every `dispatch()` is its own step,
    /// byte-for-byte as before Phase 11.
    open_turn: Option<Vec<Command>>,
    /// Phase 43 (LAT-02, D-07): monotonic counter bumped on every
    /// state-changing dispatch/undo/redo. Reset to 0 by `from_project`
    /// (mirrors undo/redo's own "history starts empty" rule) — a project
    /// switch already forces a full `get_snapshot` resync via
    /// `ProjectSwitched`, so seq continuity across switches has no
    /// observable benefit.
    ///
    /// It is deliberately captured and returned by the SAME method body that
    /// performs the mutation (never by a caller re-locking the `Store`
    /// afterwards) — a second lock acquisition would decouple the seq from
    /// the patch it describes and silently corrupt the renderer's
    /// missed-patch detection (threat T-43-02-01).
    seq: u64,
}

impl Store {
    /// A store around a fresh empty project (default track layout).
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a store from a previously serialized snapshot (e.g. project
    /// load). History starts empty: undo does not cross snapshot boundaries.
    pub fn from_project(project: Project) -> Self {
        Self {
            project,
            undo: Vec::new(),
            redo: Vec::new(),
            open_turn: None,
            // Phase 43 (LAT-02, D-07): a fresh load starts seq at 0, the same
            // rule the undo/redo stacks already follow.
            seq: 0,
        }
    }

    /// Phase 43 (LAT-02, D-07): the monotonic mutation counter. The renderer
    /// learns this ONCE at mirror-rehydration time (`get_current_seq`) and
    /// then tracks it from the `project:changed` envelope.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Validate → apply → record inverse → clear redo. On `Err`, state is
    /// guaranteed unchanged.
    ///
    /// The per-call contract is identical whether or not a turn is open: the
    /// caller ALWAYS gets this mutation's own individual `Patch` immediately
    /// (so an agent can see each tool call's result — including deterministic
    /// child-clip ids from split/duplicate/detach — before deciding its next
    /// call). Only the undo-stack BOOKKEEPING differs: inside an open turn the
    /// inverse joins the open group instead of becoming its own undo step
    /// (AGENT-03).
    ///
    /// Phase 43 (LAT-02) widened the return to `(Patch, base_seq, seq)`.
    /// `base_seq` is [`Store::seq`] BEFORE this mutation, `seq` is the value
    /// AFTER. Both are captured HERE — inside the same `&mut self` body that
    /// performed the mutation — precisely so no caller can re-lock the store
    /// to read `seq` separately and emit a pair that describes a DIFFERENT
    /// mutation (threat T-43-02-01). A rejected command returns `Err` before
    /// the bump, so a failed dispatch never advances `seq`.
    pub fn dispatch(&mut self, cmd: Command) -> Result<(Patch, u64, u64), CoreError> {
        let (patch, inverse) = cmd.apply(&mut self.project)?;
        match &mut self.open_turn {
            Some(group) => group.push(inverse),
            None => self.undo.push(vec![inverse]),
        }
        self.redo.clear();
        let base_seq = self.seq;
        self.seq += 1;
        Ok((patch, base_seq, self.seq))
    }

    /// Open an agent-turn transaction (AGENT-03). While a turn is open, every
    /// `dispatch()` accumulates its inverse into a single group that reverses
    /// as ONE `undo()` step — so a multi-tool-call agent turn is one atomic
    /// undo entry, never N individual undos.
    ///
    /// Idempotent: calling `begin_turn()` while a turn is already open is a
    /// no-op, so a caller that forgot to call `end_turn()` cannot fragment a
    /// turn (defense for threat T-11-03). Does NOT change the public signature
    /// of `dispatch()`/`undo()`/`redo()` — this is pure undo-stack bookkeeping
    /// layered on top of the unchanged v1 mutation path.
    pub fn begin_turn(&mut self) {
        if self.open_turn.is_none() {
            self.open_turn = Some(Vec::new());
        }
    }

    /// Close the open agent-turn transaction (AGENT-03). If the turn recorded
    /// 2+ inverses, they are reversed into apply-order and pushed as ONE undo
    /// group (so a single `undo()` reverts the whole turn). A turn with exactly
    /// one member still pushes a valid 1-member group (behaves as a normal
    /// single dispatch on undo); a turn where every call failed (0 members)
    /// pushes nothing — no empty undo step. No-op if no turn is open.
    ///
    /// Like `begin_turn()`, this changes only undo-stack bookkeeping — the
    /// public signatures of `dispatch()`/`undo()`/`redo()` are unchanged.
    pub fn end_turn(&mut self) {
        if let Some(mut group) = self.open_turn.take() {
            if !group.is_empty() {
                group.reverse();
                self.undo.push(group);
            }
        }
    }

    /// Undo the most recent step. Returns the patch describing the reverting
    /// change, or `None` if there is nothing to undo.
    ///
    /// Popping a 1-member group (a normal dispatch) returns that command's
    /// ORIGINAL specific `Patch` exactly as in v1. Popping a 2+-member group
    /// (an agent turn) applies each inverse in order, aggregates every touched
    /// id, and returns a single `Patch { kind: TurnReverted, ids: <all
    /// touched> }`.
    ///
    /// Phase 43 (LAT-02) widened the return to `(Patch, base_seq, seq)`, on
    /// the same single-lock-acquisition rule as [`Store::dispatch`]. An
    /// EMPTY-stack undo returns `None` and does NOT bump `seq` — nothing
    /// changed, so no envelope is emitted and the renderer's chain must not
    /// see a gap. The aggregate `Patch` deliberately carries no `entities`:
    /// a turn revert can touch anything, and the renderer's `None` fallback
    /// is `get_entities(ids)`, never `get_snapshot`.
    pub fn undo(&mut self) -> Option<(Patch, u64, u64)> {
        let group = self.undo.pop()?;
        let mut redo_group = Vec::with_capacity(group.len());
        let mut all_ids = Vec::new();
        let mut single_kind = None;
        for inverse in group {
            let (patch, redo_cmd) = inverse
                .apply(&mut self.project)
                .expect("undo stack invariant broken: stored inverse failed to apply");
            single_kind.get_or_insert(patch.kind);
            all_ids.extend(patch.ids);
            redo_group.push(redo_cmd);
        }
        redo_group.reverse();
        let kind = if redo_group.len() <= 1 {
            single_kind.expect("non-empty group always sets single_kind")
        } else {
            crate::command::PatchKind::TurnReverted
        };
        self.redo.push(redo_group);
        let base_seq = self.seq;
        self.seq += 1;
        Some((
            Patch {
                kind,
                ids: all_ids,
                entities: None,
            },
            base_seq,
            self.seq,
        ))
    }

    /// Redo the most recently undone step. Returns the patch, or `None` if
    /// there is nothing to redo.
    ///
    /// Symmetric with `undo()`: a 1-member group returns the command's
    /// original specific `Patch`; a 2+-member group (an agent turn) re-applies
    /// each command in order and returns a single `Patch { kind: TurnApplied,
    /// ids: <all touched> }`.
    ///
    /// Phase 43 (LAT-02) widened the return to `(Patch, base_seq, seq)` on
    /// exactly the rules documented on [`Store::undo`] — `seq` keeps counting
    /// FORWARD across undo/redo (it is a mutation counter, not a history
    /// cursor), and an empty-stack redo bumps nothing.
    pub fn redo(&mut self) -> Option<(Patch, u64, u64)> {
        let group = self.redo.pop()?;
        let mut undo_group = Vec::with_capacity(group.len());
        let mut all_ids = Vec::new();
        let mut single_kind = None;
        for cmd in group {
            let (patch, inverse) = cmd
                .apply(&mut self.project)
                .expect("redo stack invariant broken: stored command failed to apply");
            single_kind.get_or_insert(patch.kind);
            all_ids.extend(patch.ids);
            undo_group.push(inverse);
        }
        undo_group.reverse();
        let kind = if undo_group.len() <= 1 {
            single_kind.expect("non-empty group always sets single_kind")
        } else {
            crate::command::PatchKind::TurnApplied
        };
        self.undo.push(undo_group);
        let base_seq = self.seq;
        self.seq += 1;
        Some((
            Patch {
                kind,
                ids: all_ids,
                entities: None,
            },
            base_seq,
            self.seq,
        ))
    }

    /// Apply a transport command (Phase 4). DELIBERATELY not undoable:
    /// playback navigation (load/play/pause/seek/step) never touches the
    /// undo/redo stacks — undo after scrubbing must revert the last EDIT,
    /// not teleport the playhead. Same validate-then-apply guarantee as
    /// `dispatch`: on `Err`, state is unchanged.
    pub fn transport(&mut self, cmd: TransportCmd) -> Result<Playback, CoreError> {
        cmd.apply(&mut self.project)
    }

    /// Read-only view of the PROGRAM (timeline) playback state.
    pub fn playback(&self) -> &Playback {
        &self.project.playback
    }

    /// Read-only view of the SOURCE (MediaBin clip) playback state.
    pub fn source_playback(&self) -> &Playback {
        &self.project.source_playback
    }

    /// Which preview monitor is active (Source vs Program).
    pub fn preview_mode(&self) -> crate::model::PreviewMode {
        self.project.preview_mode
    }

    /// The playback of the ACTIVE preview monitor.
    pub fn active_playback(&self) -> &Playback {
        match self.project.preview_mode {
            crate::model::PreviewMode::Source => &self.project.source_playback,
            crate::model::PreviewMode::Program => &self.project.playback,
        }
    }

    /// Look up a media bin item by id (read-only). The app layer uses this
    /// to resolve path/rotation/duration for preview decodes without cloning
    /// the whole project.
    pub fn media_item(&self, id: &str) -> Option<&crate::model::MediaBinItem> {
        self.project.media_bin.iter().find(|m| m.id == id)
    }

    /// Read-only view of the timeline (Phase 5). The app layer resolves
    /// "what plays at position T" (`Timeline::top_video_active_at`) without
    /// cloning the whole project.
    pub fn timeline(&self) -> &crate::model::Timeline {
        &self.project.timeline
    }

    /// Read-only view of the canvas annotations (Phase 13 CANV-01). The app
    /// layer's visible-ink overlay reads the AUTHORITATIVE post-patch annotation
    /// list here (via a short store lock in the `project:changed` listener)
    /// without cloning the whole project — mirroring `timeline()`.
    pub fn canvas(&self) -> &crate::canvas::CanvasState {
        &self.project.canvas
    }

    /// Phase 43 (LAT-02, D-08): resolve entity ids to their CURRENT values —
    /// the bulk-mutation fallback so the renderer never needs `get_snapshot`
    /// when a `Patch` arrives with `entities: None`.
    ///
    /// Returns ONLY the named entities, in request order, never a full
    /// snapshot. An unknown id is silently OMITTED rather than erroring or
    /// substituting a different entity: the renderer resolves the shortfall
    /// by id, and a hard error here would turn a benign stale id into a
    /// failed mirror update. Follows the same narrow read-only grain as
    /// [`Store::timeline`] / [`Store::media_item`], which exist precisely so
    /// callers avoid [`Store::snapshot`].
    pub fn get_entities(&self, ids: &[String]) -> Vec<EntitySnapshot> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(item) = self.media_item(id) {
                out.push(EntitySnapshot::MediaBinItem(item.clone()));
            } else if let Some(clip) = self
                .project
                .timeline
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter())
                .find(|c| &c.id == id)
            {
                out.push(EntitySnapshot::Clip(clip.clone()));
            }
        }
        out
    }

    /// A full copy of the current project — what `get_snapshot` returns over
    /// IPC so the renderer can (re)build its read-only mirror from scratch.
    pub fn snapshot(&self) -> Project {
        PROJECT_CLONE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.project.clone()
    }

    /// The project canvas and timebase — `(width, height, fps)` — by BORROW.
    ///
    /// Phase 57 (plan 57-06, PLAY-07/D-08): `resolve_multilayer` needed exactly
    /// these three scalars and had no way to get them except
    /// [`Store::snapshot`], so it deep-cloned the entire `Project` **once per
    /// produced frame**. This follows the same narrow read-only grain as
    /// [`Store::timeline`] / [`Store::media_item`] / [`Store::canvas`], which
    /// exist for precisely this reason — so callers avoid `snapshot()`.
    pub fn project_canvas(&self) -> (u32, u32, f64) {
        (
            self.project.width,
            self.project.height,
            self.project.fps,
        )
    }

    /// Set the project's display name.
    ///
    /// **NOT a [`Command`] and NOT undoable, deliberately.** Project-lifecycle
    /// operations have lived outside the command system since Phase 26:
    /// `app_core::project::run_new_project` sets `fresh.name` directly and
    /// swaps the whole store via [`Store::from_project`], whose own doc says
    /// the swap "is NOT undoable". Phase 60.1's Save As joins that family — the
    /// document is being re-identified, not edited.
    ///
    /// It touches neither `seq` nor `undo` nor `redo`, and that is the point
    /// rather than an omission: renaming the document is not an edit to its
    /// content, so an undo taken after a Save As must still reverse the user's
    /// last real edit. A narrow, purpose-named accessor following
    /// [`Store::project_canvas`]'s precedent, so callers do not reach for
    /// `snapshot()` and a whole-`Project` round trip to change one string.
    pub fn set_project_name(&mut self, name: String) {
        self.project.name = name;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Phase 60.1 (plan 02): the project-lifecycle rename seam
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaBinItem;

    /// A real, undoable mutation — so "the rename did not disturb the history"
    /// is asserted against a history that actually exists.
    fn a_real_edit() -> Command {
        Command::AddMediaBinItem(MediaBinItem {
            id: "m-rename".to_string(),
            path: "C:/nowhere/clip.mp4".to_string(),
            media_kind: crate::model::MediaKind::Video,
            duration_us: 1_000_000,
            width: 1920,
            height: 1080,
            fps: 30.0,
            is_vfr: false,
            rotation_degrees: 0,
            has_audio: false,
            poster_path: None,
            folder: String::new(),
            display_name: None,
            is_image_sequence: false,
            reports_alpha: None,
        })
    }

    #[test]
    fn set_project_name_renames_the_document() {
        let mut store = Store::new();
        store.set_project_name("Renamed By Save As".to_string());
        assert_eq!(
            store.snapshot().name,
            "Renamed By Save As",
            "Save As re-identifies the live document; without this the TitleBar \
             and the next autosave would still be describing the old one"
        );
    }

    /// The whole reason this is a narrow accessor and not a `Command`: losing
    /// your undo history because you saved a copy is a bug, not a feature.
    #[test]
    fn set_project_name_does_not_disturb_undo_redo_or_seq() {
        let mut store = Store::new();
        store.dispatch(a_real_edit()).expect("the edit applies");
        let seq_before = store.seq();
        assert!(store.can_undo(), "fixture: there is history to lose");

        store.set_project_name("Copy".to_string());

        assert_eq!(
            store.seq(),
            seq_before,
            "renaming the document is not an edit to its content, so it must \
             not advance the mutation counter the renderer tracks patches by"
        );
        assert!(
            store.undo().is_some(),
            "undo after a rename must still reverse the user's last REAL edit"
        );
        assert!(
            store.can_redo(),
            "and the redo it produced must be there too"
        );
        assert_eq!(
            store.snapshot().name,
            "Copy",
            "and the new name survives the undo — it was never on the stack"
        );
    }
}
