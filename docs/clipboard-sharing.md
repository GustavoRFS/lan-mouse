# Clipboard Sharing Design

## Goal

Share clipboard content between Lan Mouse peers on Windows and Linux. The work is staged: the first implementation supports plain text; image data and file lists/content are follow-up stages.

## User behavior

- A visible “Share clipboard” control in the GTK frontend enables or pauses synchronization. It is enabled by default and its value is saved in the Lan Mouse configuration.
- While enabled, a local plain-text clipboard change is sent to every directly connected, authenticated peer. This includes accepted incoming sessions whose fingerprints are authorized and connected outgoing client sessions. Pointer-capture selection does not restrict clipboard recipients.
- A received text update replaces the local clipboard text. The update is not relayed to other peers. Lan Mouse does not keep clipboard history or send a clipboard snapshot on startup or when a peer reconnects; only changes observed while sharing is enabled are sent.
- Disabling sharing stops outgoing synchronization and prevents incoming updates from changing the local clipboard. It does not clear the current clipboard.
- Clipboard content is sent only over the existing authenticated DTLS peer sessions. Peers that do not advertise clipboard protocol support are not sent clipboard payloads.
- Clipboard text is UTF-8 and limited to 256 KiB per update. Only plain text is read; file URI lists are not flattened into text. Invalid, incomplete, duplicate, or oversized messages are ignored. A failure to send to one peer does not prevent delivery to other peers.

## Architecture

- The GTK frontend reads and writes the desktop clipboard through GDK on both Windows and Linux. It observes clipboard changes only while sharing is enabled and suppresses the change notification caused by applying a remote update.
- The frontend and daemon communicate clipboard state through the existing local IPC. Text is bounded before it crosses that interface.
- The daemon distributes local updates over its existing outgoing and incoming DTLS sessions. Incoming clipboard events are surfaced to the GTK frontend without forwarding them to other peers.
- Clipboard wire messages use a separate, versioned variable-length frame format. The fixed-size `ProtoEvent` format remains for mouse and keyboard input. Clipboard frames are at most 1200 bytes, split into chunks, tagged with a 128-bit update ID, reassembled with a 256 KiB per-update and 4 MiB global memory cap, and acknowledged so lost chunks can be retried. At most 64 incomplete updates are held globally; incomplete transfers expire after 30 seconds, and newer updates from the same origin supersede older pending updates.
- A small capability message is exchanged after connection establishment. Older versions ignore the unknown capability message and never receive clipboard payload frames.
- The same update ID is retained during retries and remembered in a 128-entry deduplication cache for 10 minutes. Applying a remote update does not emit a new local clipboard update, preventing echo loops.

## Stages

1. **Text:** GTK/GDK clipboard monitoring, persistent enable switch (default on), IPC, capability negotiation, bounded reliable text frames, delivery to directly connected peers, duplicate/echo handling, and documentation.
2. **Images:** identify and preserve the supported image MIME representation, transfer binary payloads using bounded chunks and checksums, and publish the received image to the local clipboard.
3. **Files:** transfer clipboard file manifests and contents into temporary local storage, publish those staged files as native clipboard file items, and clean up expired or superseded transfers.

Images and files are deliberately excluded from the text implementation plan because their clipboard formats and binary lifetime requirements differ from UTF-8 text. Their platform behavior and limits will be designed before those stages are implemented.
