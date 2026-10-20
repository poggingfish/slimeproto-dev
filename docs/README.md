# Slimeproto documentation

Start with [the introduction](src/introduction.md), or jump to
[getting started](src/getting-started.md) to run a server and create a document.
The chapters are plain Markdown, so you can read them here or as an mdBook site.

To preview the documentation locally with `mdbook`, run from the parent of
this directory:

```sh
mdbook serve docs --open
```

To build static HTML:

```sh
mdbook build docs
```

The output goes into `docs/book/`. The chapter order lives in
[`src/SUMMARY.md`](src/SUMMARY.md).
