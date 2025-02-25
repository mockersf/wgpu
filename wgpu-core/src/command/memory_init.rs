use std::{collections::hash_map::Entry, ops::Range, vec::Drain};

use hal::CommandEncoder;

use crate::{
    command::collect_zero_buffer_copies_for_clear_texture,
    device::Device,
    hub::Storage,
    id::{self, TextureId},
    init_tracker::*,
    resource::{Buffer, Texture},
    track::{ResourceTracker, TextureSelector, TextureState, TrackerSet},
    FastHashMap,
};

use super::{BakedCommands, DestroyedBufferError, DestroyedTextureError};

/// Surface that was discarded by `StoreOp::Discard` of a preceding renderpass.
/// Any read access to this surface needs to be preceded by a texture initialization.
#[derive(Clone)]
pub(crate) struct TextureSurfaceDiscard {
    pub texture: TextureId,
    pub mip_level: u32,
    pub layer: u32,
}

pub(crate) type SurfacesInDiscardState = Vec<TextureSurfaceDiscard>;

#[derive(Default)]
pub(crate) struct CommandBufferTextureMemoryActions {
    // init actions describe the tracker actions that we need to be executed before the command buffer is executed
    init_actions: Vec<TextureInitTrackerAction>,
    // discards describe all the discards that haven't been followed by init again within the command buffer
    // i.e. everything in this list resets the texture init state *after* the command buffer execution
    discards: Vec<TextureSurfaceDiscard>,
}

impl CommandBufferTextureMemoryActions {
    pub(crate) fn drain_init_actions(&mut self) -> Drain<TextureInitTrackerAction> {
        self.init_actions.drain(..)
    }

    pub(crate) fn discard(&mut self, discard: TextureSurfaceDiscard) {
        self.discards.push(discard);
    }

    // Registers a TextureInitTrackerAction.
    // Returns previously discarded surface that need to be initialized *immediately* now.
    // Only returns a non-empty list if action is MemoryInitKind::NeedsInitializedMemory.
    #[must_use]
    pub(crate) fn register_init_action<A: hal::Api>(
        &mut self,
        action: &TextureInitTrackerAction,
        texture_guard: &Storage<Texture<A>, TextureId>,
    ) -> SurfacesInDiscardState {
        let mut immediately_necessary_clears = SurfacesInDiscardState::new();

        // Note that within a command buffer we may stack arbitrary memory init actions on the same texture
        // Since we react to them in sequence, they are going to be dropped again at queue submit
        //
        // We don't need to add MemoryInitKind::NeedsInitializedMemory to init_actions if a surface is part of the discard list.
        // But that would mean splitting up the action which is more than we'd win here.
        self.init_actions
            .extend(match texture_guard.get(action.id) {
                Ok(texture) => texture.initialization_status.check_action(action),
                Err(_) => return immediately_necessary_clears, // texture no longer exists
            });

        // We expect very few discarded surfaces at any point in time which is why a simple linear search is likely best.
        // (i.e. most of the time self.discards is empty!)
        let init_actions = &mut self.init_actions;
        self.discards.retain(|discarded_surface| {
            if discarded_surface.texture == action.id
                && action.range.layer_range.contains(&discarded_surface.layer)
                && action
                    .range
                    .mip_range
                    .contains(&discarded_surface.mip_level)
            {
                if let MemoryInitKind::NeedsInitializedMemory = action.kind {
                    immediately_necessary_clears.push(discarded_surface.clone());

                    // Mark surface as implicitly initialized (this is relevant because it might have been uninitialized prior to discarding
                    init_actions.push(TextureInitTrackerAction {
                        id: discarded_surface.texture,
                        range: TextureInitRange {
                            mip_range: discarded_surface.mip_level
                                ..(discarded_surface.mip_level + 1),
                            layer_range: discarded_surface.layer..(discarded_surface.layer + 1),
                        },
                        kind: MemoryInitKind::ImplicitlyInitialized,
                    });
                }
                false
            } else {
                true
            }
        });

        immediately_necessary_clears
    }

    // Shortcut for register_init_action when it is known that the action is an implicit init, not requiring any immediate resource init.
    pub(crate) fn register_implicit_init<A: hal::Api>(
        &mut self,
        id: TextureId,
        range: TextureInitRange,
        texture_guard: &Storage<Texture<A>, TextureId>,
    ) {
        let must_be_empty = self.register_init_action(
            &TextureInitTrackerAction {
                id,
                range,
                kind: MemoryInitKind::ImplicitlyInitialized,
            },
            texture_guard,
        );
        assert!(must_be_empty.is_empty());
    }
}

// Utility function that takes discarded surfaces from register_init_action and initializes them on the spot.
// Takes care of barriers as well!
pub(crate) fn fixup_discarded_surfaces<
    A: hal::Api,
    InitIter: Iterator<Item = TextureSurfaceDiscard>,
>(
    inits: InitIter,
    encoder: &mut A::CommandEncoder,
    texture_guard: &Storage<Texture<A>, TextureId>,
    texture_tracker: &mut ResourceTracker<TextureState>,
    device: &Device<A>,
) {
    let mut zero_buffer_copy_regions = Vec::new();
    for init in inits {
        let mip_range = init.mip_level..(init.mip_level + 1);
        let layer_range = init.layer..(init.layer + 1);

        let (texture, pending) = texture_tracker
            .use_replace(
                &*texture_guard,
                init.texture,
                TextureSelector {
                    levels: mip_range.clone(),
                    layers: layer_range.clone(),
                },
                hal::TextureUses::COPY_DST,
            )
            .unwrap();

        collect_zero_buffer_copies_for_clear_texture(
            &texture.desc,
            device.alignments.buffer_copy_pitch.get() as u32,
            mip_range,
            layer_range,
            &mut zero_buffer_copy_regions,
        );

        let barriers = pending.map(|pending| pending.into_hal(texture));
        let raw_texture = texture.inner.as_raw().unwrap();

        unsafe {
            // TODO: Should first gather all barriers, do a single transition_textures call, and then send off copy_buffer_to_texture commands.
            encoder.transition_textures(barriers);
            encoder.copy_buffer_to_texture(
                &device.zero_buffer,
                raw_texture,
                zero_buffer_copy_regions.drain(..),
            );
        }
    }
}

impl<A: hal::Api> BakedCommands<A> {
    // inserts all buffer initializations that are going to be needed for executing the commands and updates resource init states accordingly
    pub(crate) fn initialize_buffer_memory(
        &mut self,
        device_tracker: &mut TrackerSet,
        buffer_guard: &mut Storage<Buffer<A>, id::BufferId>,
    ) -> Result<(), DestroyedBufferError> {
        // Gather init ranges for each buffer so we can collapse them.
        // It is not possible to do this at an earlier point since previously executed command buffer change the resource init state.
        let mut uninitialized_ranges_per_buffer = FastHashMap::default();
        for buffer_use in self.buffer_memory_init_actions.drain(..) {
            let buffer = buffer_guard
                .get_mut(buffer_use.id)
                .map_err(|_| DestroyedBufferError(buffer_use.id))?;

            // align the end to 4
            let end_remainder = buffer_use.range.end % wgt::COPY_BUFFER_ALIGNMENT;
            let end = if end_remainder == 0 {
                buffer_use.range.end
            } else {
                buffer_use.range.end + wgt::COPY_BUFFER_ALIGNMENT - end_remainder
            };
            let uninitialized_ranges = buffer
                .initialization_status
                .drain(buffer_use.range.start..end);

            match buffer_use.kind {
                MemoryInitKind::ImplicitlyInitialized => {}
                MemoryInitKind::NeedsInitializedMemory => {
                    match uninitialized_ranges_per_buffer.entry(buffer_use.id) {
                        Entry::Vacant(e) => {
                            e.insert(
                                uninitialized_ranges.collect::<Vec<Range<wgt::BufferAddress>>>(),
                            );
                        }
                        Entry::Occupied(mut e) => {
                            e.get_mut().extend(uninitialized_ranges);
                        }
                    }
                }
            }
        }

        for (buffer_id, mut ranges) in uninitialized_ranges_per_buffer {
            // Collapse touching ranges.
            ranges.sort_by_key(|r| r.start);
            for i in (1..ranges.len()).rev() {
                assert!(ranges[i - 1].end <= ranges[i].start); // The memory init tracker made sure of this!
                if ranges[i].start == ranges[i - 1].end {
                    ranges[i - 1].end = ranges[i].end;
                    ranges.swap_remove(i); // Ordering not important at this point
                }
            }

            // Don't do use_replace since the buffer may already no longer have a ref_count.
            // However, we *know* that it is currently in use, so the tracker must already know about it.
            let transition = device_tracker.buffers.change_replace_tracked(
                id::Valid(buffer_id),
                (),
                hal::BufferUses::COPY_DST,
            );

            let buffer = buffer_guard
                .get_mut(buffer_id)
                .map_err(|_| DestroyedBufferError(buffer_id))?;
            let raw_buf = buffer.raw.as_ref().ok_or(DestroyedBufferError(buffer_id))?;

            unsafe {
                self.encoder
                    .transition_buffers(transition.map(|pending| pending.into_hal(buffer)));
            }

            for range in ranges.iter() {
                assert!(range.start % wgt::COPY_BUFFER_ALIGNMENT == 0, "Buffer {:?} has an uninitialized range with a start not aligned to 4 (start was {})", raw_buf, range.start);
                assert!(range.end % wgt::COPY_BUFFER_ALIGNMENT == 0, "Buffer {:?} has an uninitialized range with an end not aligned to 4 (end was {})", raw_buf, range.end);

                unsafe {
                    self.encoder.clear_buffer(raw_buf, range.clone());
                }
            }
        }
        Ok(())
    }

    // inserts all texture initializations that are going to be needed for executing the commands and updates resource init states accordingly
    // any textures that are left discarded by this command buffer will be marked as uninitialized
    pub(crate) fn initialize_texture_memory(
        &mut self,
        device_tracker: &mut TrackerSet,
        texture_guard: &mut Storage<Texture<A>, TextureId>,
        device: &Device<A>,
    ) -> Result<(), DestroyedTextureError> {
        let mut ranges: Vec<TextureInitRange> = Vec::new();
        for texture_use in self.texture_memory_actions.drain_init_actions() {
            let texture = texture_guard
                .get_mut(texture_use.id)
                .map_err(|_| DestroyedTextureError(texture_use.id))?;

            let use_range = texture_use.range;
            let affected_mip_trackers = texture
                .initialization_status
                .mips
                .iter_mut()
                .enumerate()
                .skip(use_range.mip_range.start as usize)
                .take((use_range.mip_range.end - use_range.mip_range.start) as usize);

            match texture_use.kind {
                MemoryInitKind::ImplicitlyInitialized => {
                    for (_, mip_tracker) in affected_mip_trackers {
                        mip_tracker.drain(use_range.layer_range.clone());
                    }
                }
                MemoryInitKind::NeedsInitializedMemory => {
                    ranges.clear();
                    for (mip_level, mip_tracker) in affected_mip_trackers {
                        for layer_range in mip_tracker.drain(use_range.layer_range.clone()) {
                            ranges.push(TextureInitRange {
                                mip_range: mip_level as u32..(mip_level as u32 + 1),
                                layer_range,
                            })
                        }
                    }

                    let raw_texture = texture
                        .inner
                        .as_raw()
                        .ok_or(DestroyedTextureError(texture_use.id))?;

                    let mut texture_barriers = Vec::new();
                    let mut zero_buffer_copy_regions = Vec::new();
                    for range in &ranges {
                        // Don't do use_replace since the texture may already no longer have a ref_count.
                        // However, we *know* that it is currently in use, so the tracker must already know about it.
                        texture_barriers.extend(
                            device_tracker
                                .textures
                                .change_replace_tracked(
                                    id::Valid(texture_use.id),
                                    TextureSelector {
                                        levels: range.mip_range.clone(),
                                        layers: range.layer_range.clone(),
                                    },
                                    hal::TextureUses::COPY_DST,
                                )
                                .map(|pending| pending.into_hal(texture)),
                        );

                        collect_zero_buffer_copies_for_clear_texture(
                            &texture.desc,
                            device.alignments.buffer_copy_pitch.get() as u32,
                            range.mip_range.clone(),
                            range.layer_range.clone(),
                            &mut zero_buffer_copy_regions,
                        );
                    }

                    if !zero_buffer_copy_regions.is_empty() {
                        debug_assert!(texture.hal_usage.contains(hal::TextureUses::COPY_DST),
                            "Texture needs to have the COPY_DST flag. Otherwise we can't ensure initialized memory!");
                        unsafe {
                            // TODO: Could safe on transition_textures calls by bundling barriers from *all* textures.
                            // (a bbit more tricky because a naive approach would have to borrow same texture several times then)
                            self.encoder
                                .transition_textures(texture_barriers.into_iter());
                            self.encoder.copy_buffer_to_texture(
                                &device.zero_buffer,
                                raw_texture,
                                zero_buffer_copy_regions.into_iter(),
                            );
                        }
                    }
                }
            }
        }

        // Now that all buffers/textures have the proper init state for before cmdbuf start, we discard init states for textures it left discarded after its execution.
        for surface_discard in self.texture_memory_actions.discards.iter() {
            let texture = texture_guard
                .get_mut(surface_discard.texture)
                .map_err(|_| DestroyedTextureError(surface_discard.texture))?;
            texture
                .initialization_status
                .discard(surface_discard.mip_level, surface_discard.layer);
        }

        Ok(())
    }
}
