// Link probe for cmake/WallyRust.cmake. This executable is configured but never
// built; CMake's file API reports its link line, which build.rs hands to every
// artifact cargo links. Only its link inputs matter, not this code.
int main() { return 0; }
