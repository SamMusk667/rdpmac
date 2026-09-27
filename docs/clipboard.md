# Clipboard (M4 item 4)

The clipboard is shared between the Mac and the client (MS-RDPECLIP, static channel CLIPRDR).
Text has been shared since M1; pictures and files were added on 2026-09-26.
`clipboard = true | false` (`--no-clipboard`), on by default.

## How it syncs

Each connection has a worker thread that owns the Mac's pasteboard:

- Every 0.5 seconds it checks the pasteboard's change count. When the count has changed, it reads
  the text and the picture and, if they differ from what was last exchanged, tells the client
  which formats it can provide (a format list); the client asks for the data only when it
  pastes, and only then does the server read the pasteboard again, convert and answer.
- When something is copied on the client, the client sends a format list and the server asks
  for the formats one by one right away: text first, then one picture format. The channel can
  have only one request outstanding at a time, so they are asked for in order and written to the
  pasteboard together once all have arrived (the pasteboard is cleared, then the text, PNG and
  TIFF are written). If the client copies something else meanwhile, the late answers are
  dropped.
- What is identical to what was last exchanged is neither offered nor written again, and what
  the server wrote to the pasteboard itself is not offered back, so the two sides cannot bounce
  it back and forth.

## Pictures

- Mac → client: `CF_DIB` and a registered format named `PNG` are offered. The PNG keeps
  transparency; the DIB is 24-bit, composited over white, because Windows programs disagree
  about the fourth byte of a 32-bit DIB: some take it as transparency, others as padding.
  Windows synthesises `CF_BITMAP` and `CF_DIBV5` from `CF_DIB` by itself.
- Client → Mac: when the client offers `PNG`, the PNG is asked for (screenshot tools, browsers
  and Office all put one there); otherwise `CF_DIBV5`, and failing that `CF_DIB`. DIBs are
  supported from BITMAPINFOHEADER to BITMAPV5HEADER, 1 to 32 bits, uncompressed or with bit
  fields, rows either way up; at 32 bits, a fourth byte that is 0 throughout is taken as opaque.
  RLE compression and OS/2 headers are not supported.
- On the Mac both PNG and TIFF are written: newer programs read PNG, older ones TIFF. When
  reading, PNG is preferred, else TIFF; colours are converted to sRGB through ImageIO.
- A file copied in Finder brings its file icon onto the pasteboard (as TIFF); that is not
  offered as a picture.
- Pictures are limited to about 8K×4K (35 million pixels); larger ones are neither offered nor
  taken.

## Files

Files go across only when the client negotiates file copy (`CB_STREAM_FILECLIP_ENABLED`; mstsc
does); otherwise files copied in Finder still go as their names only. The capabilities the
server announces: file copy, no absolute paths, clipboard locking, files over 4 GB.

- Mac → client: copy files or folders in Finder, paste them in Explorer. The server expands the
  file URLs on the pasteboard one by one (Finder gives file reference URLs, which are resolved
  to paths first): folders are walked recursively and are listed themselves too, so empty
  folders come across as well; symbolic links, `.DS_Store` and files whose names start with `._`
  are skipped; characters Windows does not allow (`<>:"/\|?*`) become `_`, and trailing dots and
  spaces are removed; names longer than 259 characters including the relative path are skipped;
  a copy has at most 10 000 entries. The client asks by index, first for a file's size and then
  for its contents piece by piece; the server reads them from disk and answers, keeping the file
  open from one piece to the next. When the client locks the clipboard, the server keeps the
  file list of that moment, so the client can still read it to the end even if something else is
  copied on the Mac while it is locked.
- Client → Mac: copy in Explorer, paste in Finder. As soon as the client copies, the server asks
  for the file list (IronRDP parses it, cleans up the paths and locks the client's clipboard
  automatically), then fetches each file 1 MB at a time into
  `~/Library/Caches/rdpmac/clipboard/<time>/`, creating the folders as they were and keeping
  modification times. Once everything has arrived, the top-level files and folders go on the
  pasteboard, and they can then be pasted in Finder. During the download the pasteboard is
  cleared, so that its previous contents cannot be pasted. Starting a new transfer removes the
  previous transfer's folder (Finder copied the files out of it when pasting).
- If, during the download, something else is copied on the client or on the Mac, or the client
  does not answer for 30 seconds, the transfer is given up. Paths that fall outside the target
  folder (`..`, absolute paths) are skipped.
- For now, files are downloaded as soon as they are copied ("eager"). Downloading only when
  Finder pastes ("lazy"), as macrdp does, needs NSFileCoordinator and is left for later; until
  then, a very large file copied in Explorer is downloaded even if it is never pasted.

Two known issues with Explorer (recorded by macrdp; this is how Windows and mstsc behave):

- When a folder itself is copied, Explorer puts only a Shell IDList on the clipboard and
  delay-renders `FileGroupDescriptorW`; mstsc does not ask for it, so nothing comes across.
  Workaround: open the folder, select everything with Ctrl+A, and copy.
- Archivers such as 7-Zip and WinRAR intercept copies of `.zip`, `.7z`, `.rar` and similar files,
  and mstsc gets no file list. Workaround: change the extension first.

IronRDP patch: a clipboard operation that failed in the server's event loop (for example a file
list sent although the client has not negotiated file copy, or data sent for a request the client
has already given up on) used to disconnect the whole session; now only that one message is
dropped, and the log says `Dropping clipboard event`.

## Logs

- `copied on the Mac, offering it to the client chars=… picture=…`
- `copied on the client, now on the Mac chars=… picture=… written=…`
- A picture that cannot be read: `client clipboard picture could not be read bytes=…`
- `files copied on the Mac, offering them to the client files=… entries=…`
- `files copied on the client, fetching them entries=…`; when done,
  `files copied on the client arrived entries=… bytes=… seconds=… dir=…` and
  `files copied on the client, now on the Mac`; when given up midway,
  `stopped fetching files copied on the client why=…`

## Tests

- Unit tests: DIBs written (24-bit, rows bottom to top, composited over white) and read back; a
  V5 header with transparency read in as premultiplied; at 32 bits, a fourth byte that is 0
  throughout taken as opaque; bit-field masks after the header, palettes, 1-bit bitmaps;
  truncated DIBs, RLE and OS/2 headers rejected; PNG and TIFF round trips through ImageIO. The
  sync logic: a Mac picture offered as DIB and PNG, text and a picture as three formats; the
  client's text and picture asked for in order and written together; the DIB asked for when
  there is no PNG; the pasteboard left alone when the picture cannot be had; late answers
  dropped.
- The real pasteboard (a uniquely named one, so the user's clipboard is not touched): text and a
  picture written as PNG and TIFF and read back; a picture written from a PNG keeps the identity
  of the original; an unreadable PNG writes nothing; no picture offered when files are copied;
  files written as URLs and read back as paths.
- Files: a folder tree (with a file of just over 2 MB spanning three pieces, an empty folder, and
  a symbolic link and a `.DS_Store` that are skipped) plus a file with a colon in its name,
  answered by `Outgoing` and downloaded by `Incoming`, arrive with the same contents, structure
  and modification times; the size is asked for first when the client does not give it; the
  transfer stops when the client answers with an error; paths leaving the target folder are
  skipped; a locked list stays readable after a new copy, and unlocking goes back to the current
  list; the sync logic: without file copy negotiated, only names go across; with it, files are
  offered and content requests answered; the client's files clear the pasteboard first, go on it
  once all have arrived, and are not sent back; a new copy stops the download.
- Loopback on the same Mac cannot test this: the client and the server share one pasteboard.
  Verify with mstsc on another machine.
- On macOS 27, when ImageIO meets data it does not recognise, `CGImageSourceCreateImageAtIndex`
  traps and ends the process instead of returning NULL; the type and the picture count are
  checked before decoding.

## Next steps

- Lazy download for client → Mac (fetched only when Finder pastes), and showing download
  progress.
- Requesting several pieces at once, for speed on high-latency links (now one piece at a time,
  1 MB per round trip).
