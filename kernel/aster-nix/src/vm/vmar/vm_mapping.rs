// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use aster_frame::vm::{VmFrame, VmFrameVec, VmIo, VmMapOptions, VmPerm, VmSpace};

use super::{interval::Interval, is_intersected, Vmar, VmarInner};
use crate::{
    prelude::*,
    vm::{
        perms::VmPerms,
        vmar::Rights,
        vmo::{get_page_idx_range, Vmo, VmoChildOptions, VmoRightsOp},
    },
};

/// A VmMapping represents mapping a vmo into a vmar.
/// A vmar can has multiple VmMappings, which means multiple vmos are mapped to a vmar.
/// A vmo can also contain multiple VmMappings, which means a vmo can be mapped to multiple vmars.
/// The relationship between Vmar and Vmo is M:N.
pub struct VmMapping {
    inner: Mutex<VmMappingInner>,
    vm_space: VmSpace,
    /// The mapped vmo. The mapped vmo is with dynamic capability.
    vmo: Vmo<Rights>,
}

impl VmMapping {
    pub fn try_clone(&self) -> Result<Self> {
        let inner = self.inner.lock().clone();
        let vmo = self.vmo.dup()?;
        Ok(Self {
            inner: Mutex::new(inner),
            vm_space: self.vm_space.clone(),
            vmo,
        })
    }
}

#[derive(Clone)]
struct VmMappingInner {
    /// The map offset of the vmo, in bytes.
    vmo_offset: usize,
    /// The size of mapping, in bytes. The map size can even be larger than the size of vmo.
    /// Those pages outside vmo range cannot be read or write.
    map_size: usize,
    /// The base address relative to the root vmar where the vmo is mapped.
    map_to_addr: Vaddr,
    /// is destroyed
    is_destroyed: bool,
    /// The pages already mapped. The key is the page index in vmo.
    mapped_pages: BTreeSet<usize>,
    /// The permission of pages in the mapping.
    /// All pages within the same VmMapping have the same permission.
    perm: VmPerm,
}

impl Interval<usize> for Arc<VmMapping> {
    fn range(&self) -> Range<usize> {
        self.map_to_addr()..self.map_to_addr() + self.map_size()
    }
}

impl VmMapping {
    pub fn build_mapping<R1, R2>(option: VmarMapOptions<R1, R2>) -> Result<Self>
    where
        Vmo<R2>: VmoRightsOp,
    {
        let VmarMapOptions {
            parent,
            vmo,
            perms,
            vmo_offset,
            size,
            offset,
            align,
            can_overwrite,
        } = option;
        let Vmar(parent_vmar, _) = parent;
        let vmo_size = vmo.size();
        let map_to_addr = parent_vmar.allocate_free_region_for_vmo(
            vmo_size,
            size,
            offset,
            align,
            can_overwrite,
        )?;
        trace!(
            "build mapping, map_range = 0x{:x}- 0x{:x}",
            map_to_addr,
            map_to_addr + size
        );

        let vm_mapping_inner = VmMappingInner {
            vmo_offset,
            map_size: size,
            map_to_addr,
            is_destroyed: false,
            mapped_pages: BTreeSet::new(),
            perm: VmPerm::from(perms),
        };

        Ok(Self {
            inner: Mutex::new(vm_mapping_inner),
            vm_space: parent_vmar.vm_space.clone(),
            vmo: vmo.to_dyn(),
        })
    }

    /// Build a new VmMapping based on part of current `VmMapping`.
    /// The mapping range of the new mapping must be contained in the full mapping.
    ///
    /// Note: Since such new mappings will intersect with the current mapping,
    /// making sure that when adding the new mapping into a Vmar, the current mapping in the Vmar will be removed.
    fn clone_partial(
        &self,
        range: Range<usize>,
        new_perm: Option<VmPerm>,
    ) -> Result<Arc<VmMapping>> {
        let partial_mapping = Arc::new(self.try_clone()?);
        // Adjust the mapping range and the permission.
        {
            let mut inner = partial_mapping.inner.lock();
            inner.shrink_to(range);
            if let Some(perm) = new_perm {
                inner.perm = perm;
            }
        }
        Ok(partial_mapping)
    }

    pub fn vmo(&self) -> &Vmo<Rights> {
        &self.vmo
    }

    /// Set the entries in the page table associated with the current `VmMapping` to read-only.
    pub(super) fn set_pt_read_only(&self, vm_space: &VmSpace) -> Result<()> {
        let map_inner = self.inner.lock();
        let mapped_addr = &map_inner.mapped_pages;
        let perm = map_inner.perm;
        if !perm.contains(VmPerm::W) {
            return Ok(());
        }

        for page_idx in mapped_addr {
            let map_addr = map_inner.page_map_addr(*page_idx);
            vm_space.protect(&(map_addr..map_addr + PAGE_SIZE), perm - VmPerm::W)?;
        }
        Ok(())
    }

    /// Add a new committed page and map it to vmspace. If copy on write is set, it's allowed to unmap the page at the same address.
    /// FIXME: This implementation based on the truth that we map one page at a time. If multiple pages are mapped together, this implementation may have problems
    pub(super) fn map_one_page(
        &self,
        page_idx: usize,
        frame: VmFrame,
        is_readonly: bool,
    ) -> Result<()> {
        self.inner
            .lock()
            .map_one_page(&self.vmo, &self.vm_space, page_idx, frame, is_readonly)
    }

    /// unmap a page
    pub(super) fn unmap_one_page(&self, page_idx: usize) -> Result<()> {
        self.inner.lock().unmap_one_page(&self.vm_space, page_idx)
    }

    /// the mapping's start address
    pub fn map_to_addr(&self) -> Vaddr {
        self.inner.lock().map_to_addr
    }

    /// the mapping's size
    pub fn map_size(&self) -> usize {
        self.inner.lock().map_size
    }

    /// the vmo_offset
    pub fn vmo_offset(&self) -> usize {
        self.inner.lock().vmo_offset
    }

    pub fn read_bytes(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let vmo_read_offset = self.vmo_offset() + offset;

        // TODO: the current logic is vulnerable to TOCTTOU attack, since the permission may change after check.
        let page_idx_range = get_page_idx_range(&(vmo_read_offset..vmo_read_offset + buf.len()));
        let read_perm = VmPerm::R;
        for page_idx in page_idx_range {
            self.check_perm(&page_idx, &read_perm)?;
        }

        self.vmo.read_bytes(vmo_read_offset, buf)?;
        Ok(())
    }

    pub fn write_bytes(&self, offset: usize, buf: &[u8]) -> Result<()> {
        let vmo_write_offset = self.vmo_offset() + offset;

        let page_idx_range = get_page_idx_range(&(vmo_write_offset..vmo_write_offset + buf.len()));
        let write_perm = VmPerm::W;

        let mut page_addr =
            self.map_to_addr() - self.vmo_offset() + page_idx_range.start * PAGE_SIZE;
        for page_idx in page_idx_range {
            self.check_perm(&page_idx, &write_perm)?;

            let vm_space = &self.vm_space;

            // The `VmMapping` has the write permission but the corresponding PTE is present and is read-only.
            // This means this PTE is set to read-only due to the COW mechanism. In this situation we need to trigger a
            // page fault before writing at the VMO to guarantee the consistency between VMO and the page table.
            let need_page_fault = vm_space.is_mapped(page_addr) && !vm_space.is_writable(page_addr);
            if need_page_fault {
                self.handle_page_fault(page_addr, false, true)?;
            }
            page_addr += PAGE_SIZE;
        }

        self.vmo.write_bytes(vmo_write_offset, buf)?;
        Ok(())
    }

    /// Unmap pages in the range
    pub fn unmap(&self, range: &Range<usize>, may_destroy: bool) -> Result<()> {
        self.inner.lock().unmap(&self.vm_space, range, may_destroy)
    }

    pub fn is_destroyed(&self) -> bool {
        self.inner.lock().is_destroyed
    }

    pub fn handle_page_fault(
        &self,
        page_fault_addr: Vaddr,
        not_present: bool,
        write: bool,
    ) -> Result<()> {
        let vmo_offset = self.vmo_offset() + page_fault_addr - self.map_to_addr();
        if vmo_offset >= self.vmo.size() {
            return_errno_with_message!(Errno::EACCES, "page fault addr is not backed up by a vmo");
        }
        let page_idx = vmo_offset / PAGE_SIZE;
        if write {
            self.vmo.check_rights(Rights::WRITE)?;
        } else {
            self.vmo.check_rights(Rights::READ)?;
        }

        let required_perm = if write { VmPerm::W } else { VmPerm::R };
        self.check_perm(&page_idx, &required_perm)?;

        let frame = self.vmo.get_committed_frame(page_idx, write)?;

        // If read access to cow vmo triggers page fault, the map should be readonly.
        // If user next tries to write to the frame, another page fault will be triggered.
        let is_readonly = self.vmo.is_cow_vmo() && !write;
        self.map_one_page(page_idx, frame, is_readonly)
    }

    /// Protect a specified range of pages in the mapping to the target perms.
    /// The VmMapping will split to maintain its property.
    ///
    /// Since this method will modify the `vm_mappings` in the vmar,
    /// it should not be called during the direct iteration of the `vm_mappings`.
    pub(super) fn protect(
        &self,
        new_perms: VmPerms,
        range: Range<usize>,
        parent: &mut VmarInner,
    ) -> Result<()> {
        // If `new_perms` is equal to `old_perms`, `protect()` will not modify any permission in the VmMapping.
        let old_perms = VmPerms::from(self.inner.lock().perm);
        if old_perms == new_perms {
            return Ok(());
        }

        let rights = Rights::from(new_perms);
        self.vmo().check_rights(rights)?;
        // Protect permission for the perm in the VmMapping.
        self.protect_with_subdivision(&range, VmPerm::from(new_perms), parent)?;
        // Protect permission in the VmSpace.
        self.inner
            .lock()
            .protect(&self.vm_space, new_perms, range)?;

        Ok(())
    }

    pub(super) fn new_cow(&self, vm_space: VmSpace) -> Result<VmMapping> {
        let VmMapping { inner, vmo, .. } = self;

        let child_vmo = {
            let parent_vmo = vmo.dup().unwrap();
            let vmo_size = parent_vmo.size();
            VmoChildOptions::new_cow(parent_vmo, 0..vmo_size).alloc()?
        };

        let new_inner = {
            let inner = self.inner.lock();
            VmMappingInner {
                vmo_offset: inner.vmo_offset,
                map_size: inner.map_size,
                map_to_addr: inner.map_to_addr,
                is_destroyed: inner.is_destroyed,
                mapped_pages: BTreeSet::new(),
                perm: inner.perm,
            }
        };

        Ok(VmMapping {
            inner: Mutex::new(new_inner),
            vm_space,
            vmo: child_vmo,
        })
    }

    pub fn range(&self) -> Range<usize> {
        self.map_to_addr()..self.map_to_addr() + self.map_size()
    }

    /// Protect the current `VmMapping` to enforce new permissions within a specified range.
    ///
    /// Due to the property of `VmMapping`, this operation may require subdividing the current
    /// `VmMapping`. In this condition, it will generate a new `VmMapping` with the specified `perm` to protect the
    /// target range, as well as additional `VmMappings` to preserve the mappings in the remaining ranges.
    ///
    /// There are four conditions:
    /// 1. |--------old perm--------| -> |-old-| + |------new------|
    /// 2. |--------old perm--------| -> |-new-| + |------old------|
    /// 3. |--------old perm--------| -> |-old-| + |-new-| + |-old-|
    /// 4. |--------old perm--------| -> |---------new perm--------|
    ///
    /// Generally, this function is only used in `protect()` method.
    /// This method modifies the parent `Vmar` in the end if subdividing is required.
    /// It removes current mapping and add splitted mapping to the Vmar.
    fn protect_with_subdivision(
        &self,
        intersect_range: &Range<usize>,
        perm: VmPerm,
        parent: &mut VmarInner,
    ) -> Result<()> {
        let mut additional_mappings = Vec::new();
        let range = self.range();
        // Condition 4, the `additional_mappings` will be empty.
        if range.start == intersect_range.start && range.end == intersect_range.end {
            self.inner.lock().perm = perm;
            return Ok(());
        }
        // Condition 1 or 3, which needs an additional new VmMapping with range (range.start..intersect_range.start)
        if range.start < intersect_range.start {
            let additional_left_mapping =
                self.clone_partial(range.start..intersect_range.start, None)?;
            additional_mappings.push(additional_left_mapping);
        }
        // Condition 2 or 3, which needs an additional new VmMapping with range (intersect_range.end..range.end).
        if range.end > intersect_range.end {
            let additional_right_mapping =
                self.clone_partial(intersect_range.end..range.end, None)?;
            additional_mappings.push(additional_right_mapping);
        }
        // The protected VmMapping must exist and its range is `intersect_range`.
        let protected_mapping = self.clone_partial(intersect_range.clone(), Some(perm))?;

        // Begin to modify the `Vmar`.
        // Remove the original mapping.
        parent.vm_mappings.remove(&self.map_to_addr());
        // Add protected mappings to the vmar.
        parent
            .vm_mappings
            .insert(protected_mapping.map_to_addr(), protected_mapping);
        // Add additional mappings to the vmar.
        for mapping in additional_mappings {
            parent.vm_mappings.insert(mapping.map_to_addr(), mapping);
        }

        Ok(())
    }

    /// Trim a range from the mapping.
    /// There are several cases.
    /// 1. the trim_range is totally in the mapping. Then the mapping will split as two mappings.
    /// 2. the trim_range covers the mapping. Then the mapping will be destroyed.
    /// 3. the trim_range partly overlaps with the mapping, in left or right. Only overlapped part is trimmed.
    /// If we create a mapping with a new map addr, we will add it to mappings_to_append.
    /// If the mapping with map addr does not exist ever, the map addr will be added to mappings_to_remove.
    /// Otherwise, we will directly modify self.
    pub fn trim_mapping(
        self: &Arc<Self>,
        trim_range: &Range<usize>,
        mappings_to_remove: &mut BTreeSet<Vaddr>,
        mappings_to_append: &mut BTreeMap<Vaddr, Arc<VmMapping>>,
    ) -> Result<()> {
        let map_to_addr = self.map_to_addr();
        let map_size = self.map_size();
        let range = self.range();
        if !is_intersected(&range, trim_range) {
            return Ok(());
        }
        if trim_range.start <= map_to_addr && trim_range.end >= map_to_addr + map_size {
            // Fast path: the whole mapping was trimed.
            self.unmap(trim_range, true)?;
            mappings_to_remove.insert(map_to_addr);
            return Ok(());
        }
        if trim_range.start <= range.start {
            mappings_to_remove.insert(map_to_addr);
            if trim_range.end <= range.end {
                // Overlap vm_mapping from left.
                let new_map_addr = self.trim_left(trim_range.end)?;
                mappings_to_append.insert(new_map_addr, self.clone());
            } else {
                // The mapping was totally destroyed.
            }
        } else {
            if trim_range.end <= range.end {
                // The trim range was totally inside the old mapping.
                let another_mapping = Arc::new(self.try_clone()?);
                let another_map_to_addr = another_mapping.trim_left(trim_range.end)?;
                mappings_to_append.insert(another_map_to_addr, another_mapping);
            } else {
                // Overlap vm_mapping from right.
            }
            self.trim_right(trim_range.start)?;
        }

        Ok(())
    }

    /// Trim the mapping from left to a new address.
    fn trim_left(&self, vaddr: Vaddr) -> Result<Vaddr> {
        self.inner.lock().trim_left(&self.vm_space, vaddr)
    }

    /// Trim the mapping from right to a new address.
    fn trim_right(&self, vaddr: Vaddr) -> Result<Vaddr> {
        self.inner.lock().trim_right(&self.vm_space, vaddr)
    }

    fn check_perm(&self, page_idx: &usize, perm: &VmPerm) -> Result<()> {
        self.inner.lock().check_perm(page_idx, perm)
    }
}

impl VmMappingInner {
    fn map_one_page(
        &mut self,
        vmo: &Vmo<Rights>,
        vm_space: &VmSpace,
        page_idx: usize,
        frame: VmFrame,
        is_readonly: bool,
    ) -> Result<()> {
        let map_addr = self.page_map_addr(page_idx);

        let vm_perm = {
            let mut perm = self.perm;
            if is_readonly {
                debug_assert!(vmo.is_cow_vmo());
                perm -= VmPerm::W;
            }
            perm
        };

        let vm_map_options = {
            let mut options = VmMapOptions::new();
            options.addr(Some(map_addr));
            options.perm(vm_perm);
            options
        };

        // Cow child allows unmapping the mapped page.
        if vmo.is_cow_vmo() && vm_space.is_mapped(map_addr) {
            vm_space.unmap(&(map_addr..(map_addr + PAGE_SIZE))).unwrap();
        }

        vm_space.map(VmFrameVec::from_one_frame(frame), &vm_map_options)?;
        self.mapped_pages.insert(page_idx);
        Ok(())
    }

    fn unmap_one_page(&mut self, vm_space: &VmSpace, page_idx: usize) -> Result<()> {
        let map_addr = self.page_map_addr(page_idx);
        let range = map_addr..(map_addr + PAGE_SIZE);
        if vm_space.is_mapped(map_addr) {
            vm_space.unmap(&range)?;
        }
        self.mapped_pages.remove(&page_idx);
        Ok(())
    }

    /// Unmap pages in the range.
    fn unmap(&mut self, vm_space: &VmSpace, range: &Range<usize>, may_destroy: bool) -> Result<()> {
        let map_to_addr = self.map_to_addr;
        let vmo_map_range = (range.start - map_to_addr + self.vmo_offset)
            ..(range.end - map_to_addr + self.vmo_offset);
        let page_idx_range = get_page_idx_range(&vmo_map_range);
        for page_idx in page_idx_range {
            self.unmap_one_page(vm_space, page_idx)?;
        }
        if may_destroy && *range == self.range() {
            self.is_destroyed = false;
        }
        Ok(())
    }

    fn page_map_addr(&self, page_idx: usize) -> usize {
        page_idx * PAGE_SIZE + self.map_to_addr - self.vmo_offset
    }

    pub(super) fn protect(
        &mut self,
        vm_space: &VmSpace,
        perms: VmPerms,
        range: Range<usize>,
    ) -> Result<()> {
        debug_assert!(range.start % PAGE_SIZE == 0);
        debug_assert!(range.end % PAGE_SIZE == 0);
        let start_page = (range.start - self.map_to_addr + self.vmo_offset) / PAGE_SIZE;
        let end_page = (range.end - self.map_to_addr + self.vmo_offset) / PAGE_SIZE;
        let perm = VmPerm::from(perms);
        for page_idx in start_page..end_page {
            let page_addr = self.page_map_addr(page_idx);
            if vm_space.is_mapped(page_addr) {
                // If the page is already mapped, we will modify page table
                let page_range = page_addr..(page_addr + PAGE_SIZE);
                vm_space.protect(&page_range, perm)?;
            }
        }
        Ok(())
    }

    /// Trim the mapping from left to a new address.
    fn trim_left(&mut self, vm_space: &VmSpace, vaddr: Vaddr) -> Result<Vaddr> {
        trace!(
            "trim left: range: {:x?}, vaddr = 0x{:x}",
            self.range(),
            vaddr
        );
        debug_assert!(vaddr >= self.map_to_addr && vaddr <= self.map_to_addr + self.map_size);
        debug_assert!(vaddr % PAGE_SIZE == 0);
        let trim_size = vaddr - self.map_to_addr;

        self.map_to_addr = vaddr;
        let old_vmo_offset = self.vmo_offset;
        self.vmo_offset += trim_size;
        self.map_size -= trim_size;
        for page_idx in old_vmo_offset / PAGE_SIZE..self.vmo_offset / PAGE_SIZE {
            if self.mapped_pages.remove(&page_idx) {
                let _ = self.unmap_one_page(vm_space, page_idx);
            }
        }
        Ok(self.map_to_addr)
    }

    /// Trim the mapping from right to a new address.
    fn trim_right(&mut self, vm_space: &VmSpace, vaddr: Vaddr) -> Result<Vaddr> {
        trace!(
            "trim right: range: {:x?}, vaddr = 0x{:x}",
            self.range(),
            vaddr
        );
        debug_assert!(vaddr >= self.map_to_addr && vaddr <= self.map_to_addr + self.map_size);
        debug_assert!(vaddr % PAGE_SIZE == 0);
        let page_idx_range = (vaddr - self.map_to_addr + self.vmo_offset) / PAGE_SIZE
            ..(self.map_size + self.vmo_offset) / PAGE_SIZE;
        for page_idx in page_idx_range {
            let _ = self.unmap_one_page(vm_space, page_idx);
        }
        self.map_size = vaddr - self.map_to_addr;
        Ok(self.map_to_addr)
    }

    /// Shrink the current `VmMapping` to the new range.
    /// The new range must be contained in the old range.
    fn shrink_to(&mut self, new_range: Range<usize>) {
        debug_assert!(self.map_to_addr <= new_range.start);
        debug_assert!(self.map_to_addr + self.map_size >= new_range.end);
        self.vmo_offset += new_range.start - self.map_to_addr;
        self.map_to_addr = new_range.start;
        self.map_size = new_range.end - new_range.start;
    }

    fn range(&self) -> Range<usize> {
        self.map_to_addr..self.map_to_addr + self.map_size
    }

    fn check_perm(&self, page_idx: &usize, perm: &VmPerm) -> Result<()> {
        // Check if the page is in current VmMapping.
        if page_idx * PAGE_SIZE < self.vmo_offset
            || (page_idx + 1) * PAGE_SIZE > self.vmo_offset + self.map_size
        {
            return_errno_with_message!(Errno::EINVAL, "invalid page idx");
        }
        if !self.perm.contains(*perm) {
            return_errno_with_message!(Errno::EACCES, "perm check fails");
        }

        Ok(())
    }
}

/// Options for creating a new mapping. The mapping is not allowed to overlap
/// with any child VMARs. And unless specified otherwise, it is not allowed
/// to overlap with any existing mapping, either.
pub struct VmarMapOptions<R1, R2> {
    parent: Vmar<R1>,
    vmo: Vmo<R2>,
    perms: VmPerms,
    vmo_offset: usize,
    size: usize,
    offset: Option<usize>,
    align: usize,
    can_overwrite: bool,
}

impl<R1, R2> VmarMapOptions<R1, R2> {
    /// Creates a default set of options with the VMO and the memory access
    /// permissions.
    ///
    /// The VMO must have access rights that correspond to the memory
    /// access permissions. For example, if `perms` contains `VmPerm::Write`,
    /// then `vmo.rights()` should contain `Rights::WRITE`.
    pub fn new(parent: Vmar<R1>, vmo: Vmo<R2>, perms: VmPerms) -> Self {
        let size = vmo.size();
        Self {
            parent,
            vmo,
            perms,
            vmo_offset: 0,
            size,
            offset: None,
            align: PAGE_SIZE,
            can_overwrite: false,
        }
    }

    /// Sets the offset of the first memory page in the VMO that is to be
    /// mapped into the VMAR.
    ///
    /// The offset must be page-aligned and within the VMO.
    ///
    /// The default value is zero.
    pub fn vmo_offset(mut self, offset: usize) -> Self {
        self.vmo_offset = offset;
        self
    }

    /// Sets the size of the mapping.
    ///
    /// The size of a mapping may not be equal to that of the VMO.
    /// For example, it is ok to create a mapping whose size is larger than
    /// that of the VMO, although one cannot read from or write to the
    /// part of the mapping that is not backed by the VMO.
    /// So you may wonder: what is the point of supporting such _oversized_
    /// mappings?  The reason is two-fold.
    /// 1. VMOs are resizable. So even if a mapping is backed by a VMO whose
    /// size is equal to that of the mapping initially, we cannot prevent
    /// the VMO from shrinking.
    /// 2. Mappings are not allowed to overlap by default. As a result,
    /// oversized mappings can serve as a placeholder to prevent future
    /// mappings from occupying some particular address ranges accidentally.
    ///
    /// The default value is the size of the VMO.
    pub fn size(mut self, size: usize) -> Self {
        self.size = size;
        self
    }

    /// Sets the mapping's alignment.
    ///
    /// The default value is the page size.
    ///
    /// The provided alignment must be a power of two and a multiple of the
    /// page size.
    pub fn align(mut self, align: usize) -> Self {
        self.align = align;
        self
    }

    /// Sets the mapping's offset inside the VMAR.
    ///
    /// The offset must satisfy the alignment requirement.
    /// Also, the mapping's range `[offset, offset + size)` must be within
    /// the VMAR.
    ///
    /// If not set, the system will choose an offset automatically.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets whether the mapping can overwrite existing mappings.
    ///
    /// The default value is false.
    ///
    /// If this option is set to true, then the `offset` option must be
    /// set.
    pub fn can_overwrite(mut self, can_overwrite: bool) -> Self {
        self.can_overwrite = can_overwrite;
        self
    }

    /// Creates the mapping.
    ///
    /// All options will be checked at this point.
    ///
    /// On success, the virtual address of the new mapping is returned.
    pub fn build(self) -> Result<Vaddr>
    where
        Vmo<R2>: VmoRightsOp,
    {
        self.check_options()?;
        let parent_vmar = self.parent.0.clone();
        let vmo_ = self.vmo.0.clone();
        let vm_mapping = Arc::new(VmMapping::build_mapping(self)?);
        let map_to_addr = vm_mapping.map_to_addr();
        parent_vmar.add_mapping(vm_mapping);
        Ok(map_to_addr)
    }

    /// Check whether all options are valid.
    fn check_options(&self) -> Result<()>
    where
        Vmo<R2>: VmoRightsOp,
    {
        // Check align.
        debug_assert!(self.align % PAGE_SIZE == 0);
        debug_assert!(self.align.is_power_of_two());
        if self.align % PAGE_SIZE != 0 || !self.align.is_power_of_two() {
            return_errno_with_message!(Errno::EINVAL, "invalid align");
        }
        debug_assert!(self.vmo_offset % self.align == 0);
        if self.vmo_offset % self.align != 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid vmo offset");
        }
        if let Some(offset) = self.offset {
            debug_assert!(offset % self.align == 0);
            if offset % self.align != 0 {
                return_errno_with_message!(Errno::EINVAL, "invalid offset");
            }
        }
        self.check_perms()?;
        self.check_overwrite()?;
        Ok(())
    }

    /// Check whether the vmperm is subset of vmo rights.
    fn check_perms(&self) -> Result<()>
    where
        Vmo<R2>: VmoRightsOp,
    {
        let perm_rights = Rights::from(self.perms);
        self.vmo.check_rights(perm_rights)
    }

    /// Check whether the vmo will overwrite with any existing vmo or vmar.
    fn check_overwrite(&self) -> Result<()> {
        if self.can_overwrite {
            // If `can_overwrite` is set, the offset cannot be None.
            debug_assert!(self.offset.is_some());
            if self.offset.is_none() {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "offset can not be none when can overwrite is true"
                );
            }
        }
        if self.offset.is_none() {
            // If does not specify the offset, we assume the map can always find suitable free region.
            // FIXME: is this always true?
            return Ok(());
        }
        let offset = self.offset.unwrap();
        // We should spare enough space at least for the whole vmo.
        let size = self.size.max(self.vmo.size());
        let vmo_range = offset..(offset + size);
        self.parent
            .0
            .check_vmo_overwrite(vmo_range, self.can_overwrite)
    }
}
