#!/bin/sh
echo "dockerfile action ran as $INPUT_WHO with arg $1"
echo "said=hello $1" >> "$GITHUB_OUTPUT"
echo "token=$1" >> "$GITHUB_STATE"
