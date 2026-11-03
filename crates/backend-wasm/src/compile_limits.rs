//! Limits on what compiling a package can cost the host.
//!
//! Nothing can stop Wasmtime's compiler part way through, and nothing can limit the memory it
//! uses, so these are checked before it starts. The limits are on what makes compiling expensive,
//! which file size alone says little about. `docs/compile-costs.md` has the measurements behind
//! each one.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::code_shape::CodeShape;

const MB: u64 = 1024 * 1024;

/// The most that compiling one package can cost this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompileLimits {
    /// How long to wait for a package to compile, and how long to wait for an earlier one to
    /// finish first. A compile that takes too long is abandoned, but it keeps running.
    pub time: Duration,
    /// Components inside the component. Their cost multiplies, so the default is none at all.
    pub nested_components: u64,
    pub modules: u64,
    pub functions: u64,
    pub code_bytes: u64,
    pub function_bytes: u64,
    pub work: u64,
    pub nested_loops: u64,
    pub function_loops: u64,
    pub function_branch_points: u64,
    pub function_block_values: u64,
    pub instance_imports: u64,
    pub memories: u64,
    pub table_entries: u64,
    pub canonical_functions: u64,
    pub declarations: u64,
    pub declaration_bytes: u64,
    pub signature_values: u64,
}

impl Default for CompileLimits {
    /// Well above the most that real code has been measured to need. `docs/compile-costs.md` has
    /// those figures, and what a package can cost within these limits.
    fn default() -> Self {
        Self {
            time: Duration::from_secs(120),
            nested_components: 0,
            modules: 256,
            functions: 100_000,
            code_bytes: 16 * MB,
            function_bytes: MB / 2,
            work: 1_000_000_000_000,
            nested_loops: 16,
            function_loops: 1024,
            function_branch_points: 64_000,
            function_block_values: 1024,
            instance_imports: 4_000_000,
            memories: 16,
            table_entries: 1_000_000,
            canonical_functions: 2048,
            declarations: 100_000,
            declaration_bytes: 8 * MB,
            signature_values: 1_000_000,
        }
    }
}

impl CompileLimits {
    /// Says which limit a component is over, if any. The text holds only our own words and counts,
    /// so it's safe to print.
    pub fn broken_by(&self, shape: &CodeShape) -> Option<String> {
        let checks = [
            (
                "nested components",
                shape.nested_components,
                self.nested_components,
            ),
            ("modules", shape.modules, self.modules),
            (
                "imports across all module instances",
                shape.module_instances.saturating_mul(shape.module_imports),
                self.instance_imports,
            ),
            ("functions", shape.functions, self.functions),
            ("bytes of code", shape.code_bytes, self.code_bytes),
            (
                "bytes in the largest function",
                shape.largest_function,
                self.function_bytes,
            ),
            ("units of work", shape.work, self.work),
            ("nested loops", shape.deepest_loops, self.nested_loops),
            (
                "loops in one function",
                shape.most_loops,
                self.function_loops,
            ),
            (
                "branch points in one function",
                shape.most_branch_points,
                self.function_branch_points,
            ),
            (
                "values passed through blocks in one function",
                shape.most_block_values,
                self.function_block_values,
            ),
            ("memories", shape.memories, self.memories),
            ("table entries", shape.table_entries, self.table_entries),
            (
                "canonical functions",
                shape.canonical_functions,
                self.canonical_functions,
            ),
            ("declarations", shape.declarations, self.declarations),
            (
                "bytes of declarations",
                shape.declaration_bytes,
                self.declaration_bytes,
            ),
            (
                "parameters and results in signatures",
                shape.signature_values,
                self.signature_values,
            ),
        ];
        checks
            .into_iter()
            .find(|(_, found, limit)| found > limit)
            .map(|(name, found, limit)| format!("{name}: {found}, over the limit of {limit}"))
    }
}

/// Lets one package compile at a time. A compile can't be stopped, so one that's been abandoned
/// has to hold on to this until it ends. That way a hostile package can use up one core and one
/// compile's worth of memory, however many times it's loaded.
#[derive(Debug, Default)]
pub(crate) struct CompileSlot {
    in_use: Mutex<bool>,
    freed: Condvar,
}

impl CompileSlot {
    /// Waits for the slot to be free, or returns nothing if it isn't within the time given.
    pub(crate) fn claim(self: &Arc<Self>, wait: Duration) -> Option<ClaimedSlot> {
        let deadline = Instant::now() + wait;
        let mut in_use = self.lock();
        while *in_use {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            in_use = self
                .freed
                .wait_timeout(in_use, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *in_use = true;
        Some(ClaimedSlot(Arc::clone(self)))
    }

    // A panic can't leave the flag wrong, because it's only ever set whole.
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.in_use.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Frees the slot when dropped, which also happens if the compiler panics.
#[derive(Debug)]
pub(crate) struct ClaimedSlot(Arc<CompileSlot>);

impl Drop for ClaimedSlot {
    fn drop(&mut self) {
        *self.0.lock() = false;
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each limit, as (how to set it, how to give a shape that many, words in the refusal).
    type Limit = (
        fn(&mut CompileLimits) -> &mut u64,
        fn(&mut CodeShape) -> &mut u64,
        &'static str,
    );
    const LIMITS: [Limit; 16] = [
        (
            |l| &mut l.nested_components,
            |s| &mut s.nested_components,
            "nested components",
        ),
        (|l| &mut l.modules, |s| &mut s.modules, "modules"),
        (|l| &mut l.functions, |s| &mut s.functions, "functions"),
        (
            |l| &mut l.code_bytes,
            |s| &mut s.code_bytes,
            "bytes of code",
        ),
        (
            |l| &mut l.function_bytes,
            |s| &mut s.largest_function,
            "bytes in the largest function",
        ),
        (|l| &mut l.work, |s| &mut s.work, "units of work"),
        (
            |l| &mut l.nested_loops,
            |s| &mut s.deepest_loops,
            "nested loops",
        ),
        (
            |l| &mut l.function_loops,
            |s| &mut s.most_loops,
            "loops in one function",
        ),
        (
            |l| &mut l.function_branch_points,
            |s| &mut s.most_branch_points,
            "branch points",
        ),
        (
            |l| &mut l.function_block_values,
            |s| &mut s.most_block_values,
            "passed through blocks",
        ),
        (|l| &mut l.memories, |s| &mut s.memories, "memories"),
        (
            |l| &mut l.table_entries,
            |s| &mut s.table_entries,
            "table entries",
        ),
        (
            |l| &mut l.canonical_functions,
            |s| &mut s.canonical_functions,
            "canonical functions",
        ),
        (
            |l| &mut l.declarations,
            |s| &mut s.declarations,
            "declarations",
        ),
        (
            |l| &mut l.declaration_bytes,
            |s| &mut s.declaration_bytes,
            "bytes of declarations",
        ),
        (
            |l| &mut l.signature_values,
            |s| &mut s.signature_values,
            "in signatures",
        ),
    ];

    #[test]
    fn the_default_time_limit_is_the_one_in_the_docs() {
        assert_eq!(CompileLimits::default().time, Duration::from_secs(120));
    }

    #[test]
    fn nothing_at_all_is_within_the_limits() {
        assert_eq!(
            CompileLimits::default().broken_by(&CodeShape::default()),
            None
        );
    }

    #[test]
    fn each_limit_allows_up_to_its_value_and_no_more() {
        for (limit, count, words) in LIMITS {
            let mut limits = CompileLimits::default();
            *limit(&mut limits) = 10;

            let mut shape = CodeShape::default();
            *count(&mut shape) = 10;
            assert_eq!(limits.broken_by(&shape), None, "{words}");

            *count(&mut shape) = 11;
            let reason = limits.broken_by(&shape).expect(words);
            assert!(reason.contains(words), "{reason}");
            assert!(
                reason.contains(": 11,") && reason.contains("limit of 10"),
                "{reason}"
            );
        }
    }

    #[test]
    fn instances_and_imports_are_limited_by_what_they_come_to_together() {
        let limits = CompileLimits {
            instance_imports: 100,
            ..CompileLimits::default()
        };
        let shape = |module_instances, module_imports| CodeShape {
            module_instances,
            module_imports,
            ..CodeShape::default()
        };
        assert_eq!(limits.broken_by(&shape(100, 1)), None);
        assert_eq!(limits.broken_by(&shape(10, 10)), None);
        assert_eq!(limits.broken_by(&shape(1, 100)), None);
        for broken in [shape(101, 1), shape(11, 10), shape(1, 101)] {
            let reason = limits.broken_by(&broken).unwrap();
            assert!(reason.contains("imports across"), "{reason}");
        }
        // Too large to multiply, which still has to count as over the limit.
        assert!(limits.broken_by(&shape(u64::MAX, u64::MAX)).is_some());
    }

    #[test]
    fn a_reason_is_safe_to_print() {
        let shape = CodeShape {
            nested_components: 3,
            ..CodeShape::default()
        };
        let reason = CompileLimits::default().broken_by(&shape).unwrap();
        assert_eq!(reason, "nested components: 3, over the limit of 0");
    }

    #[test]
    fn the_slot_can_only_be_claimed_once_at_a_time() {
        let slot = Arc::new(CompileSlot::default());
        let claim = slot.claim(Duration::ZERO).unwrap();
        assert!(slot.claim(Duration::ZERO).is_none());
        assert!(slot.claim(Duration::from_millis(20)).is_none());
        drop(claim);
        assert!(slot.claim(Duration::ZERO).is_some());
    }

    #[test]
    fn waiting_for_the_slot_ends_when_it_is_freed() {
        let slot = Arc::new(CompileSlot::default());
        let claim = slot.claim(Duration::ZERO).unwrap();
        let waiter = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || slot.claim(Duration::from_secs(60)).is_some())
        };
        std::thread::sleep(Duration::from_millis(20));
        drop(claim);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn a_panic_while_holding_the_slot_frees_it() {
        let slot = Arc::new(CompileSlot::default());
        let claim = slot.claim(Duration::ZERO).unwrap();
        let result = std::thread::spawn(move || {
            let _claim = claim;
            panic!("the compiler failed");
        })
        .join();
        assert!(result.is_err());
        assert!(slot.claim(Duration::ZERO).is_some());
    }
}
