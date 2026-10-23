# OCI Pull patch to oci-client 0.18.0

Upstream: https://github.com/oras-project/rust-oci-client
License: Apache-2.0; original LICENSE retained.

This local copy exposes the raw Content-Range header in SizedStream so OCI Pull
can validate the exact start, end and total before appending any bytes. It also
preserves the requested range end when falling back to descriptor URLs. All
registry authentication, TLS, proxy and redirect handling remains upstream code.
HTTP 416 is retained as a structured status even if its body is an OCI error
envelope, allowing a safe restart instead of repeatedly requesting that range.
