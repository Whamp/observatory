# Observatory

Observatory is the single, known starting point for browser-based work produced or used by AI agents. It makes persistent visual outputs and separately running browser tools findable from one place.

## Language

**Entry**:
A named browser destination discoverable through Observatory. An entry is either an Artifact or a Service.
_Avoid_: Item, mount, output

**Artifact**:
A static, persistent, browser-viewable bundle owned by Observatory. An Artifact may be a single file or a directory of related files.
_Avoid_: Output, report, page

**Project**:
A work context rooted at a canonical directory. Service names are unique within a Project.
_Avoid_: Repository, workspace

**Service**:
A separately running interactive browser application referenced by Observatory while retaining its own behavior and state. A Service is identified by its name within one Project.
_Avoid_: Artifact, embedded app

**Target**:
A named, absolute browser URL through which a Service may be reached. Target names are unique within a Service; each Service has one primary Target and may have alternatives, without automatic selection or fallback.
_Avoid_: Endpoint, mirror

**Teardown Action**:
An optional, Project-supplied instruction that decommissions an external Service when explicitly requested. Observatory does not otherwise control the Service's runtime.
_Avoid_: Shutdown hook, automatic cleanup

**Publish**:
Make an Artifact part of Observatory's owned collection under a stable identity.
_Avoid_: Hang, put, upload

**Revision**:
An immutable published state of an Artifact. A successful replacement creates a new Revision and moves the Artifact's stable identity to it.
_Avoid_: Overwrite, mutable version
