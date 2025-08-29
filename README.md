<!--
SPDX-License-Identifier: CC0-1.0
SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>
-->

Are you building a distro that uses /usr/share/factory/etc to manage etc?
Use this handy tool to make sure changes propagate to the real /etc!

# Inner Workings

The concept is fairly straight forward. Unfortunately the code is not.

In order to track what is in /etc and who is responsible for it we use xattrs on the files.
Additionally we have an xattr on /etc itself to track when it was last updated.
This allows us to do incremental updates only touching files that have changed since the last
update.

We also support a migrate option which tries to pull an existing /etc forward under our management.

For practical reasons we currently assume btrfs as underlying filesystem.

The way this works is as follows:
- We make a snapshot of /etc as /.etc.new
- We walk /usr/share/factory/etc and for each file we check if it exists in /.etc.new
  - If it does not exist we copy it and set the appropriate xattr
  - If it does exist we check if it has our xattr
    - If it does not have our xattr we assume it's a user file and leave it alone
    - If it does have our xattr we check if the hash matches the current hash of the file
      - If the hash matches we can update the file (it has not been changed by the user)
      - If the hash does not match we skip the file, assuming the user has modified it
- After processing all files we update the last updated xattr on /.etc.new
- Finally we rename /.etc.new to /etc

# Example

```
/etc [trusted.etc.factory.last-update="1755327868000000"]
/etc/shadow []
/etc/vconsole.conf [trusted.etc.factory.sha256=0sApuhGYBzC7F66leuoyXkItaXm1/AGTjxl/lIeDNwo18=]
/etc/xdg/systemd/user [trusted.etc.factory.symlink-target="../../systemd/user"]
```

Now if the user were to change vconsole.conf they'd change the checksum.
When the current checksum is no longer the one in the xattr we ignore the file.
