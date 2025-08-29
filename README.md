<!--
SPDX-License-Identifier: CC0-1.0
SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>
-->

Are you building a distro that uses /usr/share/factory/etc to manage etc?
Use this handy tool to make sure changes propagate to the real /etc!

# Inner Workings

The concept is fairly straight forward. Unfortunately the code is not.

In order to track what is in /etc and who is responsible for it we use .etc-factory.json as metadata store.
This allows us to do incremental updates only touching files that have changed since the last update.

We also support a migrate option which tries to pull an existing /etc forward under our management.

For practical reasons we currently assume btrfs as underlying filesystem.

The way this works is as follows:
- We make a snapshot of /etc as /.etc.new
- We walk /usr/share/factory/etc and for each file we check if it exists in /.etc.new
  - If it does not exist we copy it and record it in the metadata
  - If it does exist we check if it exists in the metadata
    - If it does not exist in the metadata we assume it's a user file and leave it alone
    - If it does exist we check if the hash matches the current hash of the file
      - If the hash matches we can update the file (it has not been changed by the user)
      - If the hash does not match we skip the file, assuming the user has modified it
- After processing all files we write the new metadata to /.etc.new
- Finally we rename /.etc.new to /etc
