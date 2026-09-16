#import <Foundation/Foundation.h>

#import <mach-o/dyld.h>
#import <stdlib.h>

// MLXRuntime asks whether MLX's Metal library is present before reporting
// itself available. mlx-swift ships its shaders in a resource bundle beside the
// executable (mlx-swift_Cmlx.bundle); NSBundle knows a macOS bundle keeps
// resources under Contents/Resources. Resolve the real executable path first
// (Homebrew launches through a bin/ symlink into libexec/).
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
