## ADDED Requirements

### Requirement: The Obsidian plugin is a registered device kind

`identity.registered_devices` and `identity.pairing_codes` SHALL accept the device kind `obsidian_plugin`, beside `mobile`, `browser_extension` and `export_agent`. A pairing code that pins `obsidian_plugin` SHALL be redeemed only by a device that declares that kind.

#### Scenario: A plugin device is registered and authenticated

- **WHEN** a device of kind `obsidian_plugin` is registered and opens a device session
- **THEN** Edge authenticates it and forwards its requests with the kind `obsidian_plugin`, verified by `crates/public-api/tests/notes_routes.rs::device_claim_names_the_device_kind`
