//go:build !ondevice

// The default build (and any plain CGO_ENABLED=1 build without -tags
// ondevice) never links the packaged C++ kit. No engine gets installed, so
// OnDeviceEnabled reports false and Resolve returns ErrOnDeviceNotEnabled
// for a local model. Only scripts/build-ondevice.sh, which passes -tags
// ondevice, compiles engine_ondevice.go instead of this file and installs a
// real engine.
package runanywhere
