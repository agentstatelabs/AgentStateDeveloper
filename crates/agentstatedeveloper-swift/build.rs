fn main() {
    cc::Build::new()
        .file("src/parser.c")
        .file("src/scanner.c")
        .include("src")
        .compile("tree-sitter-swift");
}
