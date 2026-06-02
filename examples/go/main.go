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
	hostOpen := dll.NewProc("hvfs_host_open")
	addShare := dll.NewProc("hvfs_add_share")
	removeShare := dll.NewProc("hvfs_remove_share")
	hostClose := dll.NewProc("hvfs_host_close")
	lastError := dll.NewProc("hvfs_last_error")

	v, _, _ := abiVersion.Call()
	fmt.Printf("hvfs ABI version: %d\n", v) // expect 2

	// (1) Register the device host against an already-created compute system, by id.
	// Call this BEFORE starting the VM; pass the system's RAM (must match its config).
	hcsID, _ := syscall.BytePtrFromString("the-hcs-system-id")
	hostJSON, _ := syscall.BytePtrFromString(`{"memory_mb":512}`)
	var host uintptr
	rc, _, _ := hostOpen.Call(
		uintptr(unsafe.Pointer(hcsID)),
		uintptr(unsafe.Pointer(hostJSON)),
		uintptr(unsafe.Pointer(&host)),
	)
	if int32(rc) != 0 {
		fmt.Printf("host_open failed (%d): %s\n", int32(rc), lastErr(lastError))
		return
	}
	defer hostClose.Call(host) // tears down every remaining device + the host

	// ... the caller starts the compute system here ...

	// (2) Hot-add one share == one virtio-fs device. The caller owns instance_id
	// uniqueness; the device class is the well-known virtio-fs id (not chosen here).
	shareJSON, _ := syscall.BytePtrFromString(
		`{"tag":"ws","path":"C:\\host\\dir","instance_id":"c1c1c1c1-3333-4333-8333-333333333333"}`)
	var share uintptr
	rc, _, _ = addShare.Call(
		uintptr(unsafe.Pointer(host)),
		uintptr(unsafe.Pointer(shareJSON)),
		uintptr(unsafe.Pointer(&share)),
	)
	if int32(rc) != 0 {
		fmt.Printf("add_share failed (%d): %s\n", int32(rc), lastErr(lastError))
		return
	}
	fmt.Println("share added")

	// (3) Best-effort live remove. On current Windows this returns HVFS_ERR_UNSUPPORTED
	// (-5): the device is reclaimed when the compute system is torn down. Both -5 and
	// 0 free the share handle.
	rc, _, _ = removeShare.Call(uintptr(unsafe.Pointer(share)))
	fmt.Printf("remove_share rc=%d (0=OK, -5=UNSUPPORTED/reclaim-at-recycle)\n", int32(rc))
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
