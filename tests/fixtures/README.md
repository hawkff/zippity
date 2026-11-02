# Archive fixtures

## RAR

These archives come from libarchive at commit
`7197234b80b9983bbcaea86776555ecca66d968b`.
They are decoded copies of the matching `.rar.uu` files in
[libarchive/test](https://github.com/libarchive/libarchive/tree/7197234b80b9983bbcaea86776555ecca66d968b/libarchive/test).
See [COPYING](COPYING) for the upstream license.

The encrypted fixtures contain `a.txt` through `d.txt` with synthetic text.
The password is `password`, except for `d.txt` in non-solid archives without
encrypted filenames, which uses `password2`.

The four multipart files form one solid RAR5 archive.

## ISO images

`iso9660.iso.gz` and `udf.iso.gz` contain generated disc images under the project's
MIT license. They contain only an empty directory, `folder/file name.txt` with
`Nested ISO content.\n`, and `notes.txt` with `ISO archive preview.\n`.

The images were generated with genisoimage 1.1.11 using `-R -J`, volume label
`ZIPPITY_TEST`, and application, publisher, and preparer identifiers `ZIPPITY`.
The UDF fixture adds `-udf`. Input modification times are Unix timestamp
`1600000000`. Gzip wrapping keeps the fixtures small; verification decompresses
them with Python's standard library and requires no image-building tools.
