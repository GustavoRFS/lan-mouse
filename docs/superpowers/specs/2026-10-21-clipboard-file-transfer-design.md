# Clipboard file transfer design

## Goal

Copy files or folders on a connected Windows or Linux machine and paste them on every other connected, authorized peer while clipboard sharing is enabled, without a size limit. Content crosses the network only when it is pasted.

## Scope

- Any number of files and folders, of any size, with their directory structure. Links to files are followed; links to directories below a copied folder are skipped so a link cycle cannot make the walk endless.
- Copying announces an offer; nothing is transferred for a copy that is never pasted. Pasting again fetches the files again.
- Only the latest copy on a device can be fetched. Copying anything else, receiving a remote clipboard update, or disabling sharing withdraws the offer.
- Offers go only to directly connected authenticated peers that advertise file offer support, and are not relayed.
- A failed paste shows a desktop notification that explains the cause in Portuguese (peer unreachable or firewall, offer withdrawn, file removed or changed on the peer, permission denied, disk full, Windows path limit, version mismatch).

## Offer and transport

- The frontend sends the copied paths to its daemon. The daemon keeps them with a random 256-bit token and announces token, TCP port, item count and names in a `FileOfferAnnouncement` (at most 64 KiB) over the existing DTLS clipboard frames, using new capability and chunk frame kinds. The in-memory file bundle frames of the earlier build are no longer decoded.
- The receiving daemon adds the IP address and certificate fingerprint of the DTLS session the offer arrived on and hands the offer to its frontend.
- The daemon serves offers with TLS 1.3 on TCP, on the same port number as its DTLS listener. The server presents the lan-mouse certificate; the client accepts only the fingerprint of the DTLS session, and authorizes itself with the token. Requests are the manifest (the walked tree, parents first) and file content from an offset, streamed in frames that can carry an error for the file being sent. Connections are reused for successive files; requests are re-checked against the current offer.
- The protocol lives in the `lan-mouse-transfer` crate: blocking server and client on top of rustls, used by the daemon (server) and the frontend (client).

## Clipboard integration

- On Windows, the received offer is an OLE data object with virtual files (`FileGroupDescriptorW`, `FileContents`, `Preferred DropEffect` copy, `ZoneIdentifier` internet). Explorer reads the manifest when pasting and each file through an `IStream` while it shows its own progress dialog. The streams live in the process' multithreaded apartment so reads run on COM worker threads instead of the GTK thread. Applications that only accept file paths (`CF_HDROP`), such as browsers or Discord, cannot paste these files. Explorer does not apply the zone to pasted files in testing, so they carry no Mark-of-the-Web.
- On Linux and macOS, the received offer is a GDK content provider for `text/uri-list` and `x-special/gnome-copied-files`. The first request downloads everything into a temporary folder and is answered when the download finished, so the paste waits for the transfer. A notification announces the download. The folder is kept until the next offer, since a file manager may still be copying from it.
- A clipboard history tool that reads text types on every change (e.g. `wl-paste --type text --watch cliphist store`) requests `text/uri-list` and so starts the download right after the copy instead of on paste; a provider cannot tell who asks for the data.
- The provider also offers `application/x-lan-mouse-file-offer`, which tells the GTK and `wl-paste` clipboard watchers that the content is a peer's offer and not a local copy. Files copied locally are read as paths from `CF_HDROP`, the GDK file list, or `text/uri-list`.

## Verification

- `lan-mouse-transfer` tests run server and client over loopback: manifest order and contents, connection reuse, offsets, wrong token, wrong certificate, withdrawn offer, removed file, and requests that try to leave the copied items.
- An ignored Windows test publishes an offer and pastes it from another process, by default with a COM client that reads the virtual files like the shell does, or with Explorer itself (`LAN_MOUSE_PASTE_WITH_EXPLORER`), and compares the result byte by byte. Explorer at medium integrity pasted from an elevated publisher.
- The Linux content provider is compiled on Windows but has to be verified on a Linux desktop.
