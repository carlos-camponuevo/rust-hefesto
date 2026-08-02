#!/bin/sh
# Regenerates the decrypt test fixture. Must match ed.sh's encryption:
#   openssl enc -aes-256-cbc -pbkdf2 -salt
printf 'hello hefesto\n' | openssl enc -aes-256-cbc -pbkdf2 -salt -pass pass:forge -out "$(dirname "$0")/fixture.enc"
