# lucida-downloader

a multithreaded client for downloading music for free with
[lucida](https://lucida.to/).

<a href="https://brainmade.org/">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://brainmade.org/white-logo.svg">
    <img alt="Brainmade mark" src="https://brainmade.org/black-logo.svg">
  </picture>
</a>

## installation

```
cargo install --git https://github.com/iFanatical/lucida-downloader
```

## usage

- find the albums you want to download on https://play.qobuz.com/ (requires an
  account, but provides superior experience) or https://www.qobuz.com/shop

- run
  ```
  lucida <urls>
  ```

```
Usage: lucida [OPTIONS] [URLS]...

Arguments:
  [URLS]...  URLs to download

Options:
  -f, --file <FILE|DIR>                files to read URLs from; a directory reads every .txt file in it
  -o, --output <OUTPUT>                custom path to download to
      --force                          overwrite already downloaded files
      --group-singles                  place all artist's singles in a "Singles" directory. their covers will not be downloaded
      --album-year <ALBUM_YEAR>        use "<album> (year)" or "(year) <album>" directory name [possible values: append, prepend]
      --flatten-directories            use "<artist> - <album>" format instead of nested "<artist>/<album>" directories
      --country <COUNTRY>              country to use accounts from [default: auto]
      --no-metadata                    disable metadata embedding by lucida
      --private                        hide tracks from recent downloads on lucida
      --album-workers <ALBUM_WORKERS>  amount of albums to download simultaneously [default: 1]
      --track-workers <TRACK_WORKERS>  amount of tracks to download simultaneously for each album [default: 4]
      --skip-tracks                    skip downloading tracks in the album
      --skip-cover                     skip downloading album cover
      --cf-clearance <CF_CLEARANCE>    set the cf_clearance cookie and the User-Agent header if Cloudflare is blocking your requests
      --user-agent <USER_AGENT>        the User-Agent header to use
  -h, --help                           Print help
```

> [!NOTE]  
> remember to support your favorite artists!

## Discover Amazon artist albums

Collect album URLs from both the artist page and its Popular Albums page without downloading music or contacting Lucida. Each page reports how many unique releases it contains and how many it adds to the combined list:

```sh
lucida --discover-artist 'https://music.amazon.com/artists/B000SZHO06/mazzy-star' --save-list mazzy-star.txt
```

Review the file, then use it with the normal downloader:

```sh
lucida --cf-clearance '<cookie>' --user-agent '<agent>' -f mazzy-star.txt
```

To download several lists in one run, put them in a directory and pass it to `-f`. Every `.txt` file directly inside it is read, in name order; the tool's own `lucida-report-*` and `lucida-failed-*` files are skipped:

```sh
lucida --cf-clearance '<cookie>' --user-agent '<agent>' -f lists/
```

By default discovery includes albums. Add `--all-releases` to include singles and EPs. URLs are deduplicated by Amazon album ID; alternate editions with different IDs are retained for review. Titles and release types go to stderr. Without `--save-list`, stdout contains only URLs. Existing list files are never overwritten.

**Coverage:** this combines releases visible in Amazon's public artist-page and Popular Albums HTML, not a verified complete discography or your purchase history. Large catalogs can be truncated (a tested large artist page showed only 12 recent releases), and Amazon's public HTML did not expose a usable next-page link. The command reports this limitation. If one page fails, it warns and uses the other; if neither provides releases, it fails without writing a list. The two captured R.E.M. pages contain 19 and 20 albums respectively, yielding 27 distinct album IDs together. It does not scrape top-song links, related artists, or guest appearances as albums.

For a larger list, open the artist's Releases/See All view in your browser, load the desired releases, and save the rendered HTML. You can copy the document element's outer HTML from browser developer tools into a UTF-8 file if Save Page produces a JavaScript shell. Then run:

```sh
lucida --discover-artist 'https://music.amazon.com/artists/B000SZHO06/mazzy-star' --artist-html artist.html --all-releases --save-list releases.txt
```

The saved page must contain Amazon's release cards with title, release type, album URL, and artist URL; only cards identifying the requested artist are included. English release-type labels (Album, EP, Single) are currently supported. Only cards retained in the saved DOM can be collected; a virtualized page that removes older cards may still be incomplete. Unrecognized/empty pages fail without creating a list. Discovery uses a separate, unauthenticated HTTP client with bounded retries/timeouts, and never sends the Lucida clearance cookie to Amazon.

Discovery also accepts a Popular Albums URL, for example `https://music.amazon.com/browse/music-items/artist/B000QJJS9O/popular-albums`; it derives the artist and fetches both pages. With `--artist-html`, only the saved page is read (no network). Supply the Popular Albums URL when the saved HTML comes from that page, since its cards show artist names instead of release types.


## Standalone-track numbering

Standalone tracks first resolve their parent album when Lucida provides its URL and match the exact track URL/ID to the full album order. If that is unavailable, standalone tracks (including failed-track retry lists) use `ffprobe` to read track-number metadata from the completed temporary audio file, then add the same numeric prefix and padding as album downloads before promoting it to the final filename. Install FFmpeg/ffprobe on PATH for this feature; if it is missing, a single warning is printed at startup. No audio is re-encoded or retagged.

Existing numbered tracks on the same disc establish/check the offset used by the full-album downloader's continuous numbering. Conflicting or absent offsets, missing track tags (including `--no-metadata` downloads), or missing ffprobe produce an explicit warning and retain the unnumbered filename. The `--group-singles` option still deliberately omits numbers for grouped singles. A correctly numbered existing destination is preserved unless `--force` is set. Existing unnumbered library files are not renamed automatically.

## End-of-run report

When a batch finishes (or is stopped with Ctrl-C), a report grouped by album directory lists every failed item with its reason, tracks that lucida reports as unavailable, and downloads that need attention (such as standalone tracks kept unnumbered). If anything is listed, the report is also saved as `lucida-report-<timestamp>.txt` in the output directory, and failed album/track URLs are saved as `lucida-failed-<timestamp>.txt` for `-f`. A failed album cover adds the album URL to the retry list; rerunning it skips existing tracks. Partial `.part` files are removed when a transfer fails or is interrupted.
