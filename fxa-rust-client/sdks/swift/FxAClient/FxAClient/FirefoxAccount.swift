/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

import Foundation
import UIKit

open class FxAConfig: RustObject {
    var raw: OpaquePointer
    var wasMoved = false

    open class func release() -> FxAConfig {
        return FxAConfig(raw: fxa_get_release_config())
    }

    required public init(raw: OpaquePointer) {
        self.raw = raw
    }

    func intoRaw() -> OpaquePointer {
        self.wasMoved = true
        return self.raw
    }

    deinit {
        if !wasMoved {
            fxa_config_free(raw)
        }
    }
}

open class FirefoxAccount: RustObject {
    var raw: OpaquePointer

    // webChannelResponse is a string for now, but will probably be a JSON
    // object in the future.
    open class func from(config: FxAConfig, webChannelResponse: String) -> FirefoxAccount {
        return FirefoxAccount(raw: fxa_from_credentials(config.intoRaw(), webChannelResponse))
    }

    public init(config: FxAConfig) {
        self.raw = fxa_new(config.intoRaw())
    }

    required public init(raw: OpaquePointer) {
        self.raw = raw
    }

    func intoRaw() -> OpaquePointer {
        return self.raw
    }

    deinit {
        fxa_free(raw)
    }

    public var profile: Optional<Profile> {
        get {
            guard let pointer = fxa_profile(raw) else {
                return nil
            }
            return Profile(raw: pointer)
        }
    }

    public var getSyncKeys: Optional<SyncKeys> {
        get {
            guard let pointer = fxa_get_sync_keys(raw) else {
                return nil
            }
            let syncKeysC = pointer.pointee;
            return SyncKeys (syncKey: String(cString: syncKeysC.sync_key).hexDecodedData,
                             xcs: String(cString: syncKeysC.xcs).hexDecodedData)
        }
    }

    // Scopes is space separated for each scope.
    public func beginOAuthFlow(redirectURI: String, scopes: [String], wantsKeys: Bool) -> Optional<URL> {
        let scope = scopes.joined(separator: " ");
        guard let pointer = fxa_begin_oauth_flow(raw, redirectURI, scope, wantsKeys) else {
            return nil
        }
        return URL(string: String(cString: pointer))
    }

    public func completeOAuthFlow(code: String, state: String) -> Optional<OAuthInfo> {
        guard let pointer = fxa_complete_oauth_flow(raw, code, state) else {
            return nil
        }
        return OAuthInfo(raw: pointer)
    }
}

open class OAuthInfo {
    var raw: UnsafeMutablePointer<OAuthInfoC>

    public init(raw: UnsafeMutablePointer<OAuthInfoC>) {
        self.raw = raw
    }

    public var scopes: [String] {
        get {
            return String(cString: raw.pointee.scope).components(separatedBy: " ")
        }
    }

    public var accessToken: String {
        get {
            return String(cString: raw.pointee.access_token)
        }
    }

    public var keysJWE: Optional<String> {
        get {
            if (raw.pointee.keys_jwe == nil) {
                return nil
            }
            return String(cString: raw.pointee.keys_jwe)
        }
    }

    deinit {
        fxa_oauth_info_free(raw)
    }
}

open class Profile {
    var raw: UnsafeMutablePointer<ProfileC>

    public init(raw: UnsafeMutablePointer<ProfileC>) {
        self.raw = raw
    }

    public var uid: String {
        get {
            return String(cString: raw.pointee.uid)
        }
    }

    public var email: String {
        get {
            return String(cString: raw.pointee.email)
        }
    }

    public var avatar: String {
        get {
            return String(cString: raw.pointee.avatar)
        }
    }

    deinit {
        fxa_profile_free(raw)
    }
}

public struct SyncKeys {
    let syncKey: Data
    let xcs: Data
}
