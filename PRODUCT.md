# Product

## Register

product

## Users

Linux desktop users who keep folders in Proton Drive and want them synced without thinking about it. Technical enough to install a CLI, but the GUI exists precisely so they never have to touch a terminal afterwards. They open the window occasionally to check state or add a folder; the app otherwise lives in the tray.

## Product Purpose

NeutronSync keeps local folders and Proton Drive folders in sync, both ways, through `neutronsync-drive`, a helper built on Proton's official Drive SDK. Success is invisibility: files are just there on every machine, deletions are recoverable, and nothing destructive ever happens without positive confirmation. The GUI's job is calm supervision, folders, activity, account state, not engagement.

## Brand Personality

Calm, precise, trustworthy. A quiet systems utility, not a consumer cloud app. It is honest about its trust model (it drives Proton's own CLI and never sees credentials) and about failure states (signed out, CLI missing, sync errors) rather than hiding them.

## Anti-references

- Not a Proton clone: no Proton logo, hexagon marks, or Proton branding beyond naming the CLI it drives. The atom mark and purple accent are its own identity.
- Not a SaaS dashboard: no metrics heroes, no engagement chrome, no marketing copy inside the app.
- Not Electron-flavored: it is a native egui app and should feel like a tight desktop utility, not a web page in a frame.

## Design Principles

- **State is the interface.** The most important pixel is whether sync is healthy. Idle, syncing, signed-out, and broken-prereq states must be unmissable and distinct.
- **Failure states teach.** A missing CLI or expired session tells the user exactly what to do next, in one screen, without jargon or error spam.
- **Nothing destructive without confirmation.** Deletions are recoverable by default; the UI never buries a destructive consequence in a small label.
- **Density where data lives, air where decisions live.** Activity lists can be dense; onboarding and confirmation moments get space and one clear action.

## Accessibility & Inclusion

Dark theme only today. Body text targets >=4.5:1 contrast on panel backgrounds (white on #1C1B24 passes; dim grays are for secondary text only, never for instructions the user must read). Single accent (purple) is never the sole carrier of state: signed-out and error states pair color with text. Motion is limited to short fades (~250 ms); nothing loops.
