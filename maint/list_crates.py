#!/usr/bin/python
#
# List our crates as they appear in Cargo.toml.  Useful for scripting.

import toml.decoder
import sys, os.path

TOPDIR = os.path.split(os.path.dirname(sys.argv[0]))[0]
WORKSPACE_TOML = os.path.join(TOPDIR, "Cargo.toml")

def strip_prefix(s, prefix):
    if s.startswith(prefix):
        return s[len(prefix):]
    else:
        return s

def crate_list():
    t = toml.decoder.load(WORKSPACE_TOML)
    return list(strip_prefix(name, "crates/") for name in t['workspace']['members'])

if __name__ == '__main__':
    for item in crate_list():
        print(item)
