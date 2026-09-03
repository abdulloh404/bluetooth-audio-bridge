SHELL := /bin/sh
.DEFAULT_GOAL := help

export CHANNEL VALUE STATE

.PHONY: all help build install uninstall devices select config phone-policy \
	phone-policy-install run status volume mute enable disable

all: build

install: build

help build install uninstall devices select config phone-policy \
phone-policy-install run status volume mute enable disable:
	@./scripts/make.sh "$@"
