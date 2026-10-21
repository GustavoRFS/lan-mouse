# Clipboard image sharing design

## Goal

Copy an image on a connected Windows or Linux machine and paste that image on every other connected, authorized peer while clipboard sharing is enabled. Text behavior and the tray behavior remain available.

## Scope

- This stage transfers one image as PNG bytes. It does not treat an image file copied in Explorer or Files as an image clipboard item; file copy is the next stage.
- An image is sent only to peers that explicitly advertise image support. Older peers continue receiving text and input normally.
- The encoded PNG limit is 8 MiB. Larger images are ignored without replacing another machine's clipboard.
- Updates go only to directly connected authenticated peers. Received images are not relayed.
- The newest local clipboard change wins. Disabling sharing cancels outstanding transfers and ignores received updates.

## Clipboard integration

- On Windows, GTK reads `GdkTexture` from the clipboard and encodes it to PNG. Received PNG is decoded to `GdkTexture` and published with `GdkClipboard.set_texture()`.
- On wlroots Wayland, the existing `wl-paste --watch` observer identifies image MIME types while the window is hidden. Prefer `image/png`; otherwise read a supported image format and decode it through GDK before PNG encoding. GTK publishes received images.
- An image MIME type takes priority over plain text. A `text/uri-list` file copy is reserved for the file stage.
- Applying a received image must not trigger a new outgoing image update. Repeated notifications about one local selection must not duplicate a transfer.

## Transport and limits

- Add a separate image capability exchange to the current clipboard frame protocol. New image frames use the same authenticated DTLS connection, update IDs, chunk acknowledgements, checksum, bounded retries, and duplicate protection as text.
- Reassembly limits are 8 MiB per image and 32 MiB globally; incomplete images expire after 30 seconds. Invalid, stale, or oversized updates are discarded.
- The GTK frontend and daemon exchange PNG payloads through existing local IPC as base64 strings with explicit size checks before and after decoding.
- A completed image is acknowledged by the peer daemon after checksum and size validation. Publishing to the desktop clipboard occurs afterward in the frontend.

## Delivery and verification

- Build the Linux binary and verify local PNG copy detection and publication with a controlled image source. Confirm a connected Windows peer acknowledges a sent image after its build is updated.
- No image content is written to diagnostic logs.
- Document the image size limit and the Wayland `wl-clipboard` requirement. File transfer remains a separate implementation stage.
