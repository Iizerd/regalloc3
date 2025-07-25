//! A virtual register represents a set of live ranges and associates [`Use`]s
//! for which the register allocator will assign an [`Allocation`]. It may
//! carry different [`Value`]s over its lifetime, but always from the same
//! [`RegBank`].

use alloc::vec::Vec;
use core::ops::Index;

use self::builder::VirtRegBuilder;
use super::coalescing::Coalescing;
use super::hints::Hints;
use super::live_range::ValueSegment;
use super::spill_allocator::SpillAllocator;
use super::split_placement::SplitPlacement;
use super::uses::Uses;
use super::value_live_ranges::ValueLiveRanges;
use crate::debug_utils::display_iter;
use crate::entity::iter::Keys;
use crate::entity::packed_option::PackedOption;
use crate::entity::{CompactList, CompactListPool, PrimaryMap};
use crate::function::Function;
use crate::internal::live_range::LiveRangeSegment;
use crate::reginfo::{RegClass, RegInfo};
use crate::{Options, Stats};

pub mod builder;

entity_def! {
    /// An opaque reference to a virtual register.
    pub entity VirtReg(u32, "v");

    /// An opaque reference to a group of virtual registers that must be allocated
    /// and evicted together.
    ///
    /// All virtual registers in a group must have:
    /// - the same register class constraint, which points to a register class with
    ///   a matching group size.
    /// - different `group_index` so that the virtual register cover the entire
    ///   register group.
    pub entity VirtRegGroup(u32, "vg");
}

pub struct VirtRegData {
    /// Sorted list of live range segments for this virtual register.
    ///
    /// This is guaranteed to contain at least one segment, and no segments can
    /// have an empty live range.
    segments: CompactList<ValueSegment>,

    /// Register class that this virtual register must be allocated from.
    pub class: RegClass,

    /// If `class` refers to a class of a register groups, this is the index
    /// of the virtual register in its register group. Otherwise this must have
    /// the value 0.
    pub group_index: u8,

    /// The [`VirtRegGroup`] that this virtual register is part of, if it is
    /// in a group.
    pub group: PackedOption<VirtRegGroup>,

    /// Whether a segment in this virtual register has a fixed-register hint.
    pub has_fixed_hint: bool,

    /// The spill weight represents the use density of this virtual register.
    ///
    /// This is calculated by summing the frequency of each use and dividing it
    /// by the size of the virtual register live range.
    ///
    /// Spill weights are used in the eviction phase: a virtual register with a
    /// higher spill weight can evict one with a lower spill weight.
    ///
    /// When a virtual register is part of a group, the spill weight of each
    /// virtual register is set to the lowest spill weight of the group.
    pub spill_weight: f32,
}

/// Storage for all virtual registers in the function.
pub struct VirtRegs {
    /// Set of virtual registers to allocate.
    virt_regs: PrimaryMap<VirtReg, VirtRegData>,

    /// `CompactListPool` for virtual register segments.
    segment_pool: CompactListPool<ValueSegment>,

    /// Groups of virtual registers that are allocated/evicted together.
    ///
    /// While virtual registers are being built, this may have the value
    /// `VirtReg::reserved_value` which indicates that the virtual register for
    /// this member has not been built yet.
    groups: PrimaryMap<VirtRegGroup, CompactList<VirtReg>>,

    /// List pool for `virt_reg_groups`.
    group_pool: CompactListPool<VirtReg>,
}

impl Index<VirtReg> for VirtRegs {
    type Output = VirtRegData;

    fn index(&self, index: VirtReg) -> &Self::Output {
        &self.virt_regs[index]
    }
}

impl VirtRegs {
    pub fn new() -> VirtRegs {
        Self {
            virt_regs: PrimaryMap::new(),
            segment_pool: CompactListPool::new(),
            groups: PrimaryMap::new(),
            group_pool: CompactListPool::new(),
        }
    }

    pub fn clear(&mut self) {
        self.virt_regs.clear();
        self.segment_pool.clear();
        self.groups.clear();
        self.group_pool.clear();
    }

    /// Iterator over all virtual registers.
    pub fn virt_regs(&self) -> Keys<VirtReg> {
        self.virt_regs.keys()
    }

    /// Number of virtual registers currently defined.
    pub fn num_virt_regs(&self) -> usize {
        self.virt_regs.len()
    }

    /// Iterator over all virtual register groups.
    pub fn groups(&self) -> Keys<VirtRegGroup> {
        self.groups.keys()
    }

    /// Returns the members of a virtual register group.
    pub fn group_members(&self, group: VirtRegGroup) -> &[VirtReg] {
        self.groups[group].as_slice(&self.group_pool)
    }

    /// Sorted list of live range segments for a virtual register.
    ///
    /// All segments are guaranteed to have a non-empty live range.
    pub fn segments(&self, vreg: VirtReg) -> &[ValueSegment] {
        self.virt_regs[vreg].segments.as_slice(&self.segment_pool)
    }

    /// Creates new virtual registers from the given segments.
    pub fn create_vreg_from_segments(
        &mut self,
        segments: &mut [ValueSegment],
        func: &impl Function,
        reginfo: &impl RegInfo,
        uses: &mut Uses,
        hints: &Hints,
        virt_reg_builder: &mut VirtRegBuilder,
        coalescing: &mut Coalescing,
        stats: &mut Stats,
        options: &Options,
        new_vregs: &mut Vec<VirtReg>,
    ) {
        debug_assert!(!segments.is_empty());
        let bank = func.value_bank(segments[0].value);
        virt_reg_builder.build(
            bank,
            func,
            reginfo,
            self,
            uses,
            hints,
            coalescing,
            stats,
            options,
            None,
            Some(new_vregs),
            segments,
        );
    }

    /// Builds virtual registers from value live ranges.
    pub fn build_initial_vregs(
        &mut self,
        func: &impl Function,
        reginfo: &impl RegInfo,
        value_live_ranges: &mut ValueLiveRanges,
        coalescing: &mut Coalescing,
        uses: &mut Uses,
        hints: &Hints,
        split_placement: &SplitPlacement,
        spill_allocator: &mut SpillAllocator,
        virt_reg_builder: &mut VirtRegBuilder,
        stats: &mut Stats,
        options: &Options,
    ) {
        self.clear();
        virt_reg_builder.clear(func);
        spill_allocator.clear(func);

        for (set, mut segments) in value_live_ranges.take_all_value_sets() {
            let bank = func.value_bank(segments[0].value);
            let spillslot_size = reginfo.spillslot_size(bank);
            let live_range_union = LiveRangeSegment::new(
                segments[0].live_range.from,
                segments.last().unwrap().live_range.to,
            );
            spill_allocator.set_range(set, spillslot_size, live_range_union);
            virt_reg_builder.build(
                bank,
                func,
                reginfo,
                self,
                uses,
                hints,
                coalescing,
                stats,
                options,
                Some(split_placement),
                None,
                &mut segments,
            );
        }

        if trace_enabled!() {
            self.dump(uses, |_| true);
        }

        stat!(stats, initial_vregs, self.virt_regs.len());
        stat!(stats, initial_vreg_groups, self.groups.len());
        stat!(
            stats,
            initial_vreg_segments,
            self.virt_regs
                .values()
                .map(|vreg_data| vreg_data.segments.len())
                .sum::<usize>()
        );
    }

    /// Dumps the virtual registers to the log.
    pub fn dump(&self, uses: &Uses, filter: impl Fn(VirtReg) -> bool) {
        trace!("Virtual registers:");
        for (vreg, vreg_data) in &self.virt_regs {
            if !filter(vreg) {
                continue;
            }
            trace!(
                "  {vreg} ({}, spill_weight={}):",
                vreg_data.class, vreg_data.spill_weight,
            );
            for segment in vreg_data.segments.as_slice(&self.segment_pool) {
                segment.dump(uses);
            }
        }
        trace!("Virtual register groups:");
        for group in self.groups() {
            if !filter(self.group_members(group)[0]) {
                continue;
            }
            trace!(
                "  {group}: {}",
                display_iter(self.group_members(group), ",")
            );
        }
    }
}
