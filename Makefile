# SPDX-License-Identifier: BSD-2-Clause
# SPDX-FileCopyrightText: 2025 Harald Sitter <sitter@kde.org>

all: etc-factory

clean:
	rm -rf target

test:
	cargo test

etc-factory:
	cargo build --release

install: etc-factory
	install -D -m 755 target/release/etc-factory $(DESTDIR)/usr/lib/etc-factory
	install -D -m 644 etc-factory.service $(DESTDIR)/usr/lib/systemd/system/etc-factory.service
