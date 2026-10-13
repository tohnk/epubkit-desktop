// Keep the console window from appearing behind the app on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Images are decoded in worker processes, held to a hard memory cap, so a
/// crafted image cannot make a decoder allocate wildly: this counts what each
/// worker holds and refuses to let it pass the cap.
#[global_allocator]
static ALLOCATOR: epubkit_core::worker::CappedAllocator = epubkit_core::worker::CappedAllocator;

fn main() {
    epubkit_desktop::run()
}
