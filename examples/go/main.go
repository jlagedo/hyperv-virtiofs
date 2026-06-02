//go:build windows

// Illustrative Go consumer — NOT a published module, NOT the product's API.
// It exists to document the intended binding shape: load the DLL with
// syscall.NewLazyDLL (no cgo) and call the C ABI directly, exactly as a host like
// Atelier binds computecore.dll. Copy this pattern into your own internal package;
// do not import this directory.
//
//	go run .   # from examples/go, with hyperv_virtiofs.dll on the DLL search path
package main

import (
	"fmt"
	"path/filepath"
	"syscall"
	"unsafe"
)

func main() {
	// Load by ABSOLUTE PATH (never a bare name): the consumer is typically an
	// elevated service, so a bare name invites DLL-preloading/planting. See
	// https://go.dev/wiki/WindowsDLLs and golang.org/x/sys/windows.NewLazySystemDLL.
	dllPath, _ := filepath.Abs("hyperv_virtiofs.dll")
	dll := syscall.NewLazyDLL(dllPath)

	abiVersion := dll.NewProc("hvfs_abi_version")
	attach := dll.NewProc("hvfs_attach")
	detach := dll.NewProc("hvfs_detach")
	lastError := dll.NewProc("hvfs_last_error")

	v, _, _ := abiVersion.Call()
	fmt.Printf("hvfs ABI version: %d\n", v)

	hcsID, _ := syscall.BytePtrFromString("the-hcs-system-id")
	deviceJSON, _ := syscall.BytePtrFromString(`{"tag":"ws"}`)
	var dev uintptr

	rc, _, _ := attach.Call(
		uintptr(unsafe.Pointer(hcsID)),
		uintptr(unsafe.Pointer(deviceJSON)),
		uintptr(unsafe.Pointer(&dev)),
	)
	if int32(rc) != 0 {
		fmt.Printf("attach failed (%d): %s\n", int32(rc), lastErr(lastError))
		return // expected on the skeleton: HVFS_ERR_NOT_IMPLEMENTED (-2)
	}
	defer detach.Call(dev)
	fmt.Println("attached")
}

func lastErr(p *syscall.LazyProc) string {
	ptr, _, _ := p.Call()
	if ptr == 0 {
		return ""
	}
	// Borrowed thread-local C string; copy it out before the next ABI call.
	var b []byte
	for i := 0; ; i++ {
		c := *(*byte)(unsafe.Pointer(ptr + uintptr(i)))
		if c == 0 {
			break
		}
		b = append(b, c)
	}
	return string(b)
}
