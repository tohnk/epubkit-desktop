# Checks to make by hand

Things the audit could not check for itself: they need the real app, the
device, or a Linux machine. None of them holds up the rest of the work; each
can wait until there is a moment, or the machine, for it.

## On the Mac, in the real app

Start it from the repository with `cargo run -p epubkit-desktop`. The output
folder's `.epubkit-*.part` files are hidden: Cmd+Shift+. shows hidden files in
Finder.

- [ ] **Cmd+Q during a run.** Add a few books, Optimize, and press Cmd+Q
  partway. The footer should say "Stopping, then closing…" and the app should
  close a moment later, leaving no `.epubkit-*.part` file in the output folder.
  Pressed twice, Cmd+Q should quit at once.
- [ ] **Closing the window during a run.** As above, with the window's close
  button.
- [ ] **Closing the window with no run.** The app should quit with it, and
  not stay in the Dock. Tauri's source says it does: closing the last window
  asks the app to exit, and nothing puts it off without a run.
- [ ] **Quit from the Dock during a run.** Right-click the Dock icon, then
  Quit. The app may take a moment, then should quit, again leaving no
  `.part` file.
- [ ] **The menu.** The epubkit menu should still hold About, Services, Hide,
  Hide Others and "Quit epubkit ⌘Q", and copy and paste should still work in a
  book's title field.
- [ ] **Stop.** Stop partway through a run: the book in progress, and those
  after it, should show as stopped, in grey, and none of them be written.
- [ ] **Dropping a folder, and "choose a folder".** The books in the folder
  and its subfolders should be listed. A folder of more than 16 books should
  show "Reading N books… M read" with a Stop beneath it.
- [ ] **A large folder.** Dropping a big tree, a whole library or the home
  folder, should show "Looking for books…" and, after a moment, a Stop that
  ends the search. The footer should then say "Stopped looking for books.",
  and nothing from that drop be listed.
- [ ] **The SD card.** Dropping a folder from the Xteink's SD card should list
  no `._` files.

## On the device

- [ ] **CrossInk's series.** An EPUB 2 book with a Calibre series, optimized
  with Clean Metadata on, should still show under its series in CrossInk's
  Library, with the Series sort.

## On Linux

These need a Linux machine.

- [ ] **The Flatpak at all.** It has never been launched.
- [ ] **Dropping a folder into the Flatpak.** Whether the file transfer portal
  passes a dropped folder has not been tried. "Choose a folder" goes through
  the file chooser portal and should work either way.
- [ ] **Closing the window during a run**, in the Flatpak.
