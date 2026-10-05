# SharePoint fixtures

Real-shaped SharePoint REST responses used by the tests in
`src/audio/sharepoint_sync.rs` (`fixture_*` tests). Every `*.json` file in
`onedrive/` and `search/` is parsed, so adding a capture adds coverage with no
code change. The `synthetic-*` files are made up; captures from a real tenant
are better, because SharePoint's actual shapes are what break the parser.

| Folder / file | What it holds | Parser |
| --- | --- | --- |
| `onedrive/*.json` | OneDrive `Documents/Recordings` folder listing | `parse_onedrive_files` |
| `search/*.json` | Search REST results for video files | `parse_search_results` |
| `teams-filenames.tsv` | Teams file names and the local start time they encode | `teams_recap::metadata::from_filename` |

## Capturing a real response

Do this in your normal browser while signed in to SharePoint. The request runs
as you, on the same origin, so no tokens are copied anywhere.

1. **OneDrive listing.** Open `https://<tenant>-my.sharepoint.com/personal/<you>/`
   (your OneDrive), open DevTools → Console, and run (replace the two
   placeholders; keep the date a few weeks back):

   ```js
   const url = `${location.origin}/personal/<you>/_api/web/GetFolderByServerRelativeUrl('/personal/<you>/Documents/Recordings')/Files?$select=Name,ServerRelativeUrl,TimeCreated,Length&$filter=TimeCreated%20ge%20datetime'2026-09-01T00:00:00Z'&$orderby=TimeCreated%20desc&$top=20`;
   copy(JSON.stringify(await (await fetch(url, { headers: { Accept: 'application/json;odata=nometadata' } })).json(), null, 1));
   ```

   The response is now on your clipboard. Paste it into a file such as
   `~/Downloads/onedrive-capture.json`.

2. **Search results.** Open `https://<tenant>.sharepoint.com/` and run:

   ```js
   const q = encodeURIComponent("(filetype:mp4 OR filetype:webm) AND LastModifiedTime>=2026-09-01");
   const url = `${location.origin}/_api/search/query?querytext='${q}'&rowlimit=20&selectproperties='Title,Path,OriginalPath,DefaultEncodingURL,LastModifiedTime,Created,Size'&sortlist='LastModifiedTime:descending'`;
   copy(JSON.stringify(await (await fetch(url, { headers: { Accept: 'application/json;odata=nometadata' } })).json(), null, 1));
   ```

3. **Redact, then review.**

   ```bash
   python3 redact.py ~/Downloads/onedrive-capture.json onedrive/real-2026-10.json
   ```

   This replaces tenant hosts, personal paths, e-mail addresses and GUIDs.
   Meeting titles are kept by default because the date logic depends on their
   exact shape; add `--titles` to replace them too. **Read the output before
   committing.** Search results can include other people's names in `Title`,
   and only you can judge what your employer allows in a repository.

4. Run the tests:

   ```bash
   cargo test -p meetily --lib fixture_
   ```

## Teams file names

Add lines to `teams-filenames.tsv` whenever a recording imports with the wrong
date: the file name exactly as SharePoint shows it, a tab, then the local
start time it should get (`YYYY-MM-DDTHH:MM:SS`), or `none` when the name has
no Teams stamp.
