//! ASD's garbage-collection retention policy, shared by `asd gc` and
//! `asd-serve`'s sweep preview so the two can never disagree about what a sweep
//! keeps.
//!
//! ASD's answer differs from AgentStateGraph's `RetentionPolicy::default()` in
//! one place, `checkpoint_every`. That default keeps every hundredth commit's
//! state as a sparse checkpoint, which suits a store whose history is its
//! product. ASD's is not: nearly all of it is derived index state, rebuilt from
//! source at the git revision a milestone records. At one pin per hundred
//! commits a busy store (a million commits is not unusual — every `asd index`
//! commits) would keep thousands of full snapshots, and a sweep would reclaim
//! almost nothing.

use agentstategraph::RetentionPolicy;

/// Commits whose state a sweep keeps in full, newest first.
pub const GC_KEEP_RECENT: usize = 100;

/// Sparse checkpoints among older commits: none. See the module docs.
pub const GC_CHECKPOINT_EVERY: usize = 0;

/// The retention policy an ASD sweep runs under.
///
/// Milestones are always kept, but since AgentStateGraph v1.2.2 a milestone
/// pins state only when its checkpoint asked to. Pins distilled before then are
/// cleared explicitly, with `asd gc --unpin-legacy`, never by a policy switch —
/// a switch would also drop the pins someone deliberately asked for.
pub fn gc_policy(keep_recent: usize, checkpoint_every: usize) -> RetentionPolicy {
    RetentionPolicy {
        keep_recent,
        checkpoint_every,
        keep_milestones: true,
    }
}
