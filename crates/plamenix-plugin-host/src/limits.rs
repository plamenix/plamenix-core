//! Per-plugin wasmtime resource limits (I8.6).
//!
//! `ResourceLimits` is the host-side cap on what a plugin's
//! `wasmtime::Store` can grow to. The activator attaches an instance
//! to every store via `Store::limiter`; growth requests above the cap
//! return `Ok(false)` from the relevant trait method, which wasmtime
//! treats as "memory.grow / table.grow returned -1" inside the plugin
//! — the plugin can recover from the failure or trap, but the host
//! itself stays responsive.
//!
//! ## Default caps
//!
//! - **Linear memory: 64 MiB.** Enough for SQL-formatter / linter /
//!   small in-memory transform plugins. Large data-set processors
//!   that legitimately need more should opt up via the manifest
//!   `[runtime.limits]` table (subject to host policy in I8.5's
//!   Permissions panel).
//! - **Tables: 10 000 elements per table, 16 tables.** Generous for
//!   indirect-call dispatch in compiler-emitted code; well below the
//!   `u32::MAX` ceiling that wasmtime accepts.
//! - **Instances: 64.** A plugin world tops out at single-digit
//!   component instances today; 64 covers nested instantiation.
//! - **Memories: 4 per instance.** wasm32-wasip2 toolchains emit one
//!   memory; we keep headroom for multi-memory MVP work.
//!
//! These defaults exist as `pub const` items so callers (test code,
//! the manifest override path, downstream wrappers) can reason about
//! the baseline without duplicating literals.

use wasmtime::ResourceLimiter;

/// 64 MiB default linear-memory cap.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// 10 000 elements per table.
pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 10_000;
/// 64 component instances per plugin Store.
pub const DEFAULT_MAX_INSTANCES: usize = 64;
/// 16 tables per Store.
pub const DEFAULT_MAX_TABLES: usize = 16;
/// 4 memories per Store (wasm32-wasip2 emits 1).
pub const DEFAULT_MAX_MEMORIES: usize = 4;

/// Per-plugin resource caps enforced by wasmtime via
/// [`ResourceLimiter`].
///
/// Construct via [`ResourceLimits::default`] for the I8.6 baseline,
/// or [`ResourceLimits::builder`] for ad-hoc overrides. Manifest
/// overrides flow through [`ResourceLimits::from_manifest_override`].
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimits {
    /// Hard cap on the plugin's linear memory in bytes. Memory-grow
    /// attempts that would exceed this return `false` to the plugin.
    pub max_memory_bytes: usize,
    /// Hard cap on table element count per table.
    pub max_table_elements: usize,
    /// Hard cap on component instance count.
    pub max_instances: usize,
    /// Hard cap on table count.
    pub max_tables: usize,
    /// Hard cap on memory count.
    pub max_memories: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_tables: DEFAULT_MAX_TABLES,
            max_memories: DEFAULT_MAX_MEMORIES,
        }
    }
}

impl ResourceLimits {
    /// Returns a builder seeded with the I8.6 defaults.
    #[must_use]
    pub fn builder() -> ResourceLimitsBuilder {
        ResourceLimitsBuilder::default()
    }

    /// Applies a manifest `[runtime.limits]` override on top of the
    /// defaults. `None` leaves the corresponding field at its default.
    #[must_use]
    pub fn from_manifest_override(over: &ResourceLimitsOverride) -> Self {
        let mut limits = Self::default();
        if let Some(mib) = over.max_memory_mib {
            limits.max_memory_bytes = mib_to_bytes(mib);
        }
        if let Some(n) = over.max_table_elements {
            limits.max_table_elements = n;
        }
        if let Some(n) = over.max_instances {
            limits.max_instances = n;
        }
        if let Some(n) = over.max_tables {
            limits.max_tables = n;
        }
        if let Some(n) = over.max_memories {
            limits.max_memories = n;
        }
        limits
    }
}

/// Builder for ad-hoc `ResourceLimits` (tests, programmatic overrides).
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimitsBuilder {
    inner: ResourceLimits,
}

impl Default for ResourceLimitsBuilder {
    fn default() -> Self {
        Self {
            inner: ResourceLimits::default(),
        }
    }
}

impl ResourceLimitsBuilder {
    /// Overrides the memory cap (bytes).
    #[must_use]
    pub fn max_memory_bytes(mut self, bytes: usize) -> Self {
        self.inner.max_memory_bytes = bytes;
        self
    }

    /// Overrides the memory cap by MiB.
    #[must_use]
    pub fn max_memory_mib(mut self, mib: usize) -> Self {
        self.inner.max_memory_bytes = mib_to_bytes(mib);
        self
    }

    /// Overrides the per-table element cap.
    #[must_use]
    pub fn max_table_elements(mut self, n: usize) -> Self {
        self.inner.max_table_elements = n;
        self
    }

    /// Overrides the instance count cap.
    #[must_use]
    pub fn max_instances(mut self, n: usize) -> Self {
        self.inner.max_instances = n;
        self
    }

    /// Overrides the table count cap.
    #[must_use]
    pub fn max_tables(mut self, n: usize) -> Self {
        self.inner.max_tables = n;
        self
    }

    /// Overrides the memory count cap.
    #[must_use]
    pub fn max_memories(mut self, n: usize) -> Self {
        self.inner.max_memories = n;
        self
    }

    /// Builds the limits.
    #[must_use]
    pub fn build(self) -> ResourceLimits {
        self.inner
    }
}

impl ResourceLimiter for ResourceLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_memory_bytes {
            return Ok(false);
        }
        if let Some(max) = maximum {
            if desired > max {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.max_table_elements {
            return Ok(false);
        }
        if let Some(max) = maximum {
            if desired > max {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn instances(&self) -> usize {
        self.max_instances
    }

    fn tables(&self) -> usize {
        self.max_tables
    }

    fn memories(&self) -> usize {
        self.max_memories
    }
}

/// Optional override from `[runtime.limits]` in the plugin manifest.
/// All fields are optional — `None` leaves the corresponding default
/// untouched.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceLimitsOverride {
    /// Memory cap in MiB. Converted to bytes by
    /// [`ResourceLimits::from_manifest_override`].
    pub max_memory_mib: Option<usize>,
    /// Per-table element cap override.
    pub max_table_elements: Option<usize>,
    /// Instance count cap override.
    pub max_instances: Option<usize>,
    /// Table count cap override.
    pub max_tables: Option<usize>,
    /// Memory count cap override.
    pub max_memories: Option<usize>,
}

fn mib_to_bytes(mib: usize) -> usize {
    mib.saturating_mul(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_match_documented_values() {
        let l = ResourceLimits::default();
        assert_eq!(l.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(l.max_table_elements, 10_000);
        assert_eq!(l.max_instances, 64);
        assert_eq!(l.max_tables, 16);
        assert_eq!(l.max_memories, 4);
    }

    #[test]
    fn builder_overrides_individual_fields() {
        let l = ResourceLimits::builder()
            .max_memory_mib(128)
            .max_table_elements(5_000)
            .max_instances(8)
            .max_tables(4)
            .max_memories(2)
            .build();
        assert_eq!(l.max_memory_bytes, 128 * 1024 * 1024);
        assert_eq!(l.max_table_elements, 5_000);
        assert_eq!(l.max_instances, 8);
        assert_eq!(l.max_tables, 4);
        assert_eq!(l.max_memories, 2);
    }

    #[test]
    fn manifest_override_preserves_unset_fields() {
        let over = ResourceLimitsOverride {
            max_memory_mib: Some(32),
            ..ResourceLimitsOverride::default()
        };
        let l = ResourceLimits::from_manifest_override(&over);
        assert_eq!(l.max_memory_bytes, 32 * 1024 * 1024);
        assert_eq!(l.max_table_elements, 10_000);
        assert_eq!(l.max_instances, 64);
    }

    #[test]
    fn memory_growing_accepts_within_cap() {
        let mut l = ResourceLimits::default();
        let ok = l.memory_growing(0, 32 * 1024 * 1024, None).unwrap();
        assert!(ok);
    }

    #[test]
    fn memory_growing_rejects_above_cap() {
        let mut l = ResourceLimits::default();
        let ok = l.memory_growing(0, 128 * 1024 * 1024, None).unwrap();
        assert!(!ok);
    }

    #[test]
    fn memory_growing_honours_supplied_maximum() {
        let mut l = ResourceLimits::default();
        // Cap is 64 MiB; plugin requested-maximum 16 MiB. Asking for
        // 32 MiB violates the plugin-supplied maximum even though it
        // fits in our cap.
        let ok = l
            .memory_growing(0, 32 * 1024 * 1024, Some(16 * 1024 * 1024))
            .unwrap();
        assert!(!ok);
    }

    #[test]
    fn table_growing_rejects_above_cap() {
        let mut l = ResourceLimits::default();
        let ok = l.table_growing(0, 100_000, None).unwrap();
        assert!(!ok);
    }

    #[test]
    fn instance_table_memory_counts_match_struct() {
        let l = ResourceLimits::default();
        assert_eq!(l.instances(), 64);
        assert_eq!(l.tables(), 16);
        assert_eq!(l.memories(), 4);
    }

    #[test]
    fn mib_to_bytes_saturates_on_huge_input() {
        // usize::MAX MiB is unrepresentable; saturating-mul prevents
        // wraparound — the cap pins at usize::MAX.
        let l = ResourceLimits::builder()
            .max_memory_mib(usize::MAX)
            .build();
        assert_eq!(l.max_memory_bytes, usize::MAX);
    }
}
