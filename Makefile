fix:
	cargo clippy
	cargo fmt
	git add .
	git commit --amend