#ifndef fxa_h
#define fxa_h

/* Generated with cbindgen:0.6.0 */

typedef struct OAuthInfoC {
    char *access_token;
    char *keys_jwe;
    char *scope;
} OAuthInfoC;

typedef struct SyncKeysC {
    char *sync_key;
    char *xcs;
} SyncKeysC;

typedef struct ProfileC {
    char *uid;
    char *email;
    char *avatar;
} ProfileC;

typedef struct FirefoxAccount FirefoxAccount;
typedef struct Config Config;

/*
 * The caller should de-allocate the result using fxa_free_str after use.
 */
char *fxa_assertion_new(FirefoxAccount *fxa, const char *audience);

/*
 * The caller should de-allocate the result using fxa_free_str after use.
 */
char *fxa_begin_oauth_flow(FirefoxAccount *fxa,
                           const char *redirect_uri,
                           const char *scopes,
                           bool wants_keys);

OAuthInfoC *fxa_complete_oauth_flow(FirefoxAccount *fxa, const char *code, const char *state);

void fxa_config_free(Config *config);

void fxa_free(FirefoxAccount *fxa);

void fxa_free_str(char *s);

/*
 * Note: After calling this function, Rust will now own `config`, therefore the caller's
 * pointer should be dropped.
 */
FirefoxAccount *fxa_from_credentials(Config *config, const char *json);

Config *fxa_get_release_config(void);

SyncKeysC *fxa_get_sync_keys(FirefoxAccount *fxa);

/*
 * Note: After calling this function, Rust will now own `config`, therefore the caller's
 * pointer should be dropped.
 */
FirefoxAccount *fxa_new(Config *config);

void fxa_oauth_info_free(OAuthInfoC *ptr);

ProfileC *fxa_profile(FirefoxAccount *fxa);

void fxa_profile_free(ProfileC *ptr);

#endif /* fxa_h */
