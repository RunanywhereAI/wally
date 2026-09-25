#import <Foundation/Foundation.h>

#import <mach-o/dyld.h>
#import <stdlib.h>

// MLXRuntime asks whether MLX's Metal library is actually present before it
// reports itself available. The SDK defines this symbol only in its CocoaPods
// packaging lane, so a SwiftPM/xcodebuild consumer has to supply it — and
// answering an unconditional 1 would claim GPU support on a build whose
// shaders never got compiled.
//
// mlx-swift ships its shaders in a resource bundle beside the executable
// (mlx-swift_Cmlx.bundle). NSBundle is what knows that a macOS bundle keeps
// its resources under Contents/Resources; walking the directory by hand does
// not, which is how this first reported "unavailable" with the metallib
// sitting right there.
/// The directory holding the real executable.
///
/// NSBundle reports the path the process was launched through, which under
/// Homebrew is a symlink in bin/ pointing at libexec/. The shaders sit beside
/// the real binary, so the link has to be resolved or MLX reports itself
/// unavailable on every installed copy while working perfectly from the build
/// tree.
static NSURL *executableDirectory(void) {
    char raw[PATH_MAX];
    uint32_t size = sizeof(raw);
    if (_NSGetExecutablePath(raw, &size) != 0) {
        return NSBundle.mainBundle.executableURL.URLByDeletingLastPathComponent;
    }
    char resolved[PATH_MAX];
    const char *path = realpath(raw, resolved) != NULL ? resolved : raw;
    return [NSURL fileURLWithPath:@(path)].URLByDeletingLastPathComponent;
}

int32_t ra_mlx_metal_resource_anchor(void) {
    NSURL *directory = executableDirectory();
    if (directory == nil) {
        return 0;
    }
    // MLX tries this physical-executable-relative path before SwiftPM bundles.
    // Unlike NSBundle.mainBundle, it also works through an install symlink.
    NSURL *colocated = [directory URLByAppendingPathComponent:@"mlx.metallib"];
    if ([NSFileManager.defaultManager fileExistsAtPath:colocated.path]) {
        return 1;
    }
    if ([NSBundle.mainBundle URLForResource:@"default" withExtension:@"metallib"] != nil) {
        return 1;
    }
    NSArray<NSURL *> *entries = [NSFileManager.defaultManager
              contentsOfDirectoryAtURL:directory
            includingPropertiesForKeys:nil
                               options:NSDirectoryEnumerationSkipsHiddenFiles
                                 error:nil];
    for (NSURL *entry in entries) {
        if (![entry.pathExtension isEqualToString:@"bundle"]) {
            continue;
        }
        NSBundle *bundle = [NSBundle bundleWithURL:entry];
        if ([bundle URLForResource:@"default" withExtension:@"metallib"] != nil) {
            return 1;
        }
    }
    return 0;
}
