module notes

go 1.22

require github.com/fenecdb/fenecdb/integrations/go v0.0.0

// What `go mod init notes` and `go get` write on the Quickstart, with the SDK
// as this checkout holds it (the page's reader takes a tagged one).
// Run by ../../run-tests.sh.
replace github.com/fenecdb/fenecdb/integrations/go => ../../../go
