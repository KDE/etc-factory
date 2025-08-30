# SPDX-License-Identifier: BSD-2-Clause
# SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>

all: etc-factory

clean:
	rm -rf target

test_prepare:
	[ -d mnt ] || mkdir mnt
	[ -f test.img ] || fallocate -l 260M test.img
	umount test.img || true
	mkfs.btrfs -f test.img
	mount test.img mnt
	chown ${CHOWN_ID}:${CHOWN_ID} mnt
	cp -rv fixture/. mnt/
	chown -R ${CHOWN_ID}:${CHOWN_ID} mnt

test:
ifneq ($(shell id -u), 0)
	sudo CHOWN_ID=${USER} make test_prepare
else
	make test_prepare
endif
	cargo test

etc-factory:
	cargo build --release

install: etc-factory
	install -D -m 755 target/release/etc-factory $(DESTDIR)/usr/lib/etc-factory
	install -D -m 644 etc-factory.service $(DESTDIR)/usr/lib/systemd/system/etc-factory.service
