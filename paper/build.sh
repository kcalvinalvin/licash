#!/bin/sh
# Build the paper's PDF with TeX Live in Docker.
set -eu
cd "$(dirname "$0")"
docker run --rm -v "$PWD":/w:Z -w /w texlive/texlive:latest-medium \
  latexmk -pdf -interaction=nonstopmode -halt-on-error licash.tex
