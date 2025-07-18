use chrono::Utc;
use num_traits::FromPrimitive;
use rocket::serde::json::Json;
use rocket::{
    form::{Form, FromForm},
    Route,
};
use serde_json::Value;

use crate::{
    api::{
        core::{
            accounts::{PreloginData, RegisterData, _prelogin, _register},
            log_user_event,
            two_factor::{authenticator, duo, duo_oidc, email, enforce_2fa_policy, webauthn, yubikey},
        },
        master_password_policy,
        push::register_push_device,
        ApiResult, EmptyResult, JsonResult,
    },
    auth::{generate_organization_api_key_login_claims, ClientHeaders, ClientIp, ClientVersion},
    db::{models::*, DbConn},
    error::MapResult,
    mail, util, CONFIG,
};

pub fn routes() -> Vec<Route> {
    routes![login, prelogin, identity_register, register_verification_email, register_finish]
}

#[post("/connect/token", data = "<data>")]
async fn login(
    data: Form<ConnectData>,
    client_header: ClientHeaders,
    client_version: Option<ClientVersion>,
    mut conn: DbConn,
) -> JsonResult {
    let data: ConnectData = data.into_inner();

    let mut user_id: Option<UserId> = None;

    let login_result = match data.grant_type.as_ref() {
        "refresh_token" => {
            _check_is_some(&data.refresh_token, "refresh_token cannot be blank")?;
            _refresh_login(data, &mut conn).await
        }
        "password" => {
            _check_is_some(&data.client_id, "client_id cannot be blank")?;
            _check_is_some(&data.password, "password cannot be blank")?;
            _check_is_some(&data.scope, "scope cannot be blank")?;
            _check_is_some(&data.username, "username cannot be blank")?;

            _check_is_some(&data.device_identifier, "device_identifier cannot be blank")?;
            _check_is_some(&data.device_name, "device_name cannot be blank")?;
            _check_is_some(&data.device_type, "device_type cannot be blank")?;

            _password_login(data, &mut user_id, &mut conn, &client_header.ip, &client_version).await
        }
        "client_credentials" => {
            _check_is_some(&data.client_id, "client_id cannot be blank")?;
            _check_is_some(&data.client_secret, "client_secret cannot be blank")?;
            _check_is_some(&data.scope, "scope cannot be blank")?;

            _check_is_some(&data.device_identifier, "device_identifier cannot be blank")?;
            _check_is_some(&data.device_name, "device_name cannot be blank")?;
            _check_is_some(&data.device_type, "device_type cannot be blank")?;

            _api_key_login(data, &mut user_id, &mut conn, &client_header.ip).await
        }
        t => err!("Invalid type", t),
    };

    if let Some(user_id) = user_id {
        match &login_result {
            Ok(_) => {
                log_user_event(
                    EventType::UserLoggedIn as i32,
                    &user_id,
                    client_header.device_type,
                    &client_header.ip.ip,
                    &mut conn,
                )
                .await;
            }
            Err(e) => {
                if let Some(ev) = e.get_event() {
                    log_user_event(
                        ev.event as i32,
                        &user_id,
                        client_header.device_type,
                        &client_header.ip.ip,
                        &mut conn,
                    )
                    .await
                }
            }
        }
    }

    login_result
}

async fn _refresh_login(data: ConnectData, conn: &mut DbConn) -> JsonResult {
    // Extract token
    let token = data.refresh_token.unwrap();

    // Get device by refresh token
    let mut device = Device::find_by_refresh_token(&token, conn).await.map_res("Invalid refresh token")?;

    let scope = "api offline_access";
    let scope_vec = vec!["api".into(), "offline_access".into()];

    // Common
    let user = User::find_by_uuid(&device.user_uuid, conn).await.unwrap();
    // ---
    // Disabled this variable, it was used to generate the JWT
    // Because this might get used in the future, and is add by the Bitwarden Server, lets keep it, but then commented out
    // See: https://github.com/dani-garcia/vaultwarden/issues/4156
    // ---
    // let members = Membership::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, scope_vec, data.client_id);
    device.save(conn).await?;

    let result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "refresh_token": device.refresh_token,

        "scope": scope,
    });

    Ok(Json(result))
}

async fn _password_login(
    data: ConnectData,
    user_id: &mut Option<UserId>,
    conn: &mut DbConn,
    ip: &ClientIp,
    client_version: &Option<ClientVersion>,
) -> JsonResult {
    // Validate scope
    let scope = data.scope.as_ref().unwrap();
    if scope != "api offline_access" {
        err!("Scope not supported")
    }
    let scope_vec = vec!["api".into(), "offline_access".into()];

    // Ratelimit the login
    crate::ratelimit::check_limit_login(&ip.ip)?;

    // Get the user
    let username = data.username.as_ref().unwrap().trim();
    let Some(mut user) = User::find_by_mail(username, conn).await else {
        err!("Username or password is incorrect. Try again", format!("IP: {}. Username: {username}.", ip.ip))
    };

    // Set the user_id here to be passed back used for event logging.
    *user_id = Some(user.uuid.clone());

    // Check if the user is disabled
    if !user.enabled {
        err!(
            "This user has been disabled",
            format!("IP: {}. Username: {username}.", ip.ip),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let password = data.password.as_ref().unwrap();

    // If we get an auth request, we don't check the user's password, but the access code of the auth request
    if let Some(ref auth_request_id) = data.auth_request {
        let Some(auth_request) = AuthRequest::find_by_uuid_and_user(auth_request_id, &user.uuid, conn).await else {
            err!(
                "Auth request not found. Try again.",
                format!("IP: {}. Username: {username}.", ip.ip),
                ErrorEvent {
                    event: EventType::UserFailedLogIn,
                }
            )
        };

        let expiration_time = auth_request.creation_date + chrono::Duration::minutes(5);
        let request_expired = Utc::now().naive_utc() >= expiration_time;

        if auth_request.user_uuid != user.uuid
            || !auth_request.approved.unwrap_or(false)
            || request_expired
            || ip.ip.to_string() != auth_request.request_ip
            || !auth_request.check_access_code(password)
        {
            err!(
                "Username or access code is incorrect. Try again",
                format!("IP: {}. Username: {username}.", ip.ip),
                ErrorEvent {
                    event: EventType::UserFailedLogIn,
                }
            )
        }
    } else if !user.check_valid_password(password) {
        err!(
            "Username or password is incorrect. Try again",
            format!("IP: {}. Username: {username}.", ip.ip),
            ErrorEvent {
                event: EventType::UserFailedLogIn,
            }
        )
    }

    // Change the KDF Iterations (only when not logging in with an auth request)
    if data.auth_request.is_none() && user.password_iterations != CONFIG.password_iterations() {
        user.password_iterations = CONFIG.password_iterations();
        user.set_password(password, None, false, None);

        if let Err(e) = user.save(conn).await {
            error!("Error updating user: {e:#?}");
        }
    }

    let now = Utc::now().naive_utc();

    if user.verified_at.is_none() && CONFIG.mail_enabled() && CONFIG.signups_verify() {
        if user.last_verifying_at.is_none()
            || now.signed_duration_since(user.last_verifying_at.unwrap()).num_seconds()
                > CONFIG.signups_verify_resend_time() as i64
        {
            let resend_limit = CONFIG.signups_verify_resend_limit() as i32;
            if resend_limit == 0 || user.login_verify_count < resend_limit {
                // We want to send another email verification if we require signups to verify
                // their email address, and we haven't sent them a reminder in a while...
                user.last_verifying_at = Some(now);
                user.login_verify_count += 1;

                if let Err(e) = user.save(conn).await {
                    error!("Error updating user: {e:#?}");
                }

                if let Err(e) = mail::send_verify_email(&user.email, &user.uuid).await {
                    error!("Error auto-sending email verification email: {e:#?}");
                }
            }
        }

        // We still want the login to fail until they actually verified the email address
        err!(
            "Please verify your email before trying again.",
            format!("IP: {}. Username: {username}.", ip.ip),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let (mut device, new_device) = get_device(&data, conn, &user).await;

    let twofactor_token = twofactor_auth(&user, &data, &mut device, ip, client_version, conn).await?;

    if CONFIG.mail_enabled() && new_device {
        if let Err(e) = mail::send_new_device_logged_in(&user.email, &ip.ip.to_string(), &now, &device).await {
            error!("Error sending new device email: {e:#?}");

            if CONFIG.require_device_email() {
                err!(
                    "Could not send login notification email. Please contact your administrator.",
                    ErrorEvent {
                        event: EventType::UserFailedLogIn
                    }
                )
            }
        }
    }

    // register push device
    if !new_device {
        register_push_device(&mut device, conn).await?;
    }

    // Common
    // ---
    // Disabled this variable, it was used to generate the JWT
    // Because this might get used in the future, and is add by the Bitwarden Server, lets keep it, but then commented out
    // See: https://github.com/dani-garcia/vaultwarden/issues/4156
    // ---
    // let members = Membership::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, scope_vec, data.client_id);
    device.save(conn).await?;

    let master_password_policy = master_password_policy(&user, conn).await;

    let mut result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "refresh_token": device.refresh_token,
        "Key": user.akey,
        "PrivateKey": user.private_key,
        //"TwoFactorToken": "11122233333444555666777888999"

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false, // TODO: Same as above
        "ForcePasswordReset": false,
        "MasterPasswordPolicy": master_password_policy,

        "scope": scope,
        "UserDecryptionOptions": {
            "HasMasterPassword": !user.password_hash.is_empty(),
            "Object": "userDecryptionOptions"
        },
    });

    if let Some(token) = twofactor_token {
        result["TwoFactorToken"] = Value::String(token);
    }

    info!("User {username} logged in successfully. IP: {}", ip.ip);
    Ok(Json(result))
}

async fn _api_key_login(
    data: ConnectData,
    user_id: &mut Option<UserId>,
    conn: &mut DbConn,
    ip: &ClientIp,
) -> JsonResult {
    // Ratelimit the login
    crate::ratelimit::check_limit_login(&ip.ip)?;

    // Validate scope
    match data.scope.as_ref().unwrap().as_ref() {
        "api" => _user_api_key_login(data, user_id, conn, ip).await,
        "api.organization" => _organization_api_key_login(data, conn, ip).await,
        _ => err!("Scope not supported"),
    }
}

async fn _user_api_key_login(
    data: ConnectData,
    user_id: &mut Option<UserId>,
    conn: &mut DbConn,
    ip: &ClientIp,
) -> JsonResult {
    // Get the user via the client_id
    let client_id = data.client_id.as_ref().unwrap();
    let Some(client_user_id) = client_id.strip_prefix("user.") else {
        err!("Malformed client_id", format!("IP: {}.", ip.ip))
    };
    let client_user_id: UserId = client_user_id.into();
    let Some(user) = User::find_by_uuid(&client_user_id, conn).await else {
        err!("Invalid client_id", format!("IP: {}.", ip.ip))
    };

    // Set the user_id here to be passed back used for event logging.
    *user_id = Some(user.uuid.clone());

    // Check if the user is disabled
    if !user.enabled {
        err!(
            "This user has been disabled (API key login)",
            format!("IP: {}. Username: {}.", ip.ip, user.email),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    // Check API key. Note that API key logins bypass 2FA.
    let client_secret = data.client_secret.as_ref().unwrap();
    if !user.check_valid_api_key(client_secret) {
        err!(
            "Incorrect client_secret",
            format!("IP: {}. Username: {}.", ip.ip, user.email),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let (mut device, new_device) = get_device(&data, conn, &user).await;

    if CONFIG.mail_enabled() && new_device {
        let now = Utc::now().naive_utc();
        if let Err(e) = mail::send_new_device_logged_in(&user.email, &ip.ip.to_string(), &now, &device).await {
            error!("Error sending new device email: {e:#?}");

            if CONFIG.require_device_email() {
                err!(
                    "Could not send login notification email. Please contact your administrator.",
                    ErrorEvent {
                        event: EventType::UserFailedLogIn
                    }
                )
            }
        }
    }

    // Common
    let scope_vec = vec!["api".into()];
    // ---
    // Disabled this variable, it was used to generate the JWT
    // Because this might get used in the future, and is add by the Bitwarden Server, lets keep it, but then commented out
    // See: https://github.com/dani-garcia/vaultwarden/issues/4156
    // ---
    // let members = Membership::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, scope_vec, data.client_id);
    device.save(conn).await?;

    info!("User {} logged in successfully via API key. IP: {}", user.email, ip.ip);

    // Note: No refresh_token is returned. The CLI just repeats the
    // client_credentials login flow when the existing token expires.
    let result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "Key": user.akey,
        "PrivateKey": user.private_key,

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false, // TODO: according to official server seems something like: user.password_hash.is_empty(), but would need testing
        "scope": "api",
    });

    Ok(Json(result))
}

async fn _organization_api_key_login(data: ConnectData, conn: &mut DbConn, ip: &ClientIp) -> JsonResult {
    // Get the org via the client_id
    let client_id = data.client_id.as_ref().unwrap();
    let Some(org_id) = client_id.strip_prefix("organization.") else {
        err!("Malformed client_id", format!("IP: {}.", ip.ip))
    };
    let org_id: OrganizationId = org_id.to_string().into();
    let Some(org_api_key) = OrganizationApiKey::find_by_org_uuid(&org_id, conn).await else {
        err!("Invalid client_id", format!("IP: {}.", ip.ip))
    };

    // Check API key.
    let client_secret = data.client_secret.as_ref().unwrap();
    if !org_api_key.check_valid_api_key(client_secret) {
        err!("Incorrect client_secret", format!("IP: {}. Organization: {}.", ip.ip, org_api_key.org_uuid))
    }

    let claim = generate_organization_api_key_login_claims(org_api_key.uuid, org_api_key.org_uuid);
    let access_token = crate::auth::encode_jwt(&claim);

    Ok(Json(json!({
        "access_token": access_token,
        "expires_in": 3600,
        "token_type": "Bearer",
        "scope": "api.organization",
    })))
}

/// Retrieves an existing device or creates a new device from ConnectData and the User
async fn get_device(data: &ConnectData, conn: &mut DbConn, user: &User) -> (Device, bool) {
    // On iOS, device_type sends "iOS", on others it sends a number
    // When unknown or unable to parse, return 14, which is 'Unknown Browser'
    let device_type = util::try_parse_string(data.device_type.as_ref()).unwrap_or(14);
    let device_id = data.device_identifier.clone().expect("No device id provided");
    let device_name = data.device_name.clone().expect("No device name provided");

    let mut new_device = false;
    // Find device or create new
    let device = match Device::find_by_uuid_and_user(&device_id, &user.uuid, conn).await {
        Some(device) => device,
        None => {
            new_device = true;
            Device::new(device_id, user.uuid.clone(), device_name, device_type)
        }
    };

    (device, new_device)
}

async fn twofactor_auth(
    user: &User,
    data: &ConnectData,
    device: &mut Device,
    ip: &ClientIp,
    client_version: &Option<ClientVersion>,
    conn: &mut DbConn,
) -> ApiResult<Option<String>> {
    let twofactors = TwoFactor::find_by_user(&user.uuid, conn).await;

    // No twofactor token if twofactor is disabled
    if twofactors.is_empty() {
        enforce_2fa_policy(user, &user.uuid, device.atype, &ip.ip, conn).await?;
        return Ok(None);
    }

    TwoFactorIncomplete::mark_incomplete(&user.uuid, &device.uuid, &device.name, device.atype, ip, conn).await?;

    let twofactor_ids: Vec<_> = twofactors.iter().map(|tf| tf.atype).collect();
    let selected_id = data.two_factor_provider.unwrap_or(twofactor_ids[0]); // If we aren't given a two factor provider, assume the first one

    let twofactor_code = match data.two_factor_token {
        Some(ref code) => code,
        None => {
            err_json!(
                _json_err_twofactor(&twofactor_ids, &user.uuid, data, client_version, conn).await?,
                "2FA token not provided"
            )
        }
    };

    let selected_twofactor = twofactors.into_iter().find(|tf| tf.atype == selected_id && tf.enabled);

    use crate::crypto::ct_eq;

    let selected_data = _selected_data(selected_twofactor);
    let mut remember = data.two_factor_remember.unwrap_or(0);

    match TwoFactorType::from_i32(selected_id) {
        Some(TwoFactorType::Authenticator) => {
            authenticator::validate_totp_code_str(&user.uuid, twofactor_code, &selected_data?, ip, conn).await?
        }
        Some(TwoFactorType::Webauthn) => webauthn::validate_webauthn_login(&user.uuid, twofactor_code, conn).await?,
        Some(TwoFactorType::YubiKey) => yubikey::validate_yubikey_login(twofactor_code, &selected_data?).await?,
        Some(TwoFactorType::Duo) => {
            match CONFIG.duo_use_iframe() {
                true => {
                    // Legacy iframe prompt flow
                    duo::validate_duo_login(&user.email, twofactor_code, conn).await?
                }
                false => {
                    // OIDC based flow
                    duo_oidc::validate_duo_login(
                        &user.email,
                        twofactor_code,
                        data.client_id.as_ref().unwrap(),
                        data.device_identifier.as_ref().unwrap(),
                        conn,
                    )
                    .await?
                }
            }
        }
        Some(TwoFactorType::Email) => {
            email::validate_email_code_str(&user.uuid, twofactor_code, &selected_data?, &ip.ip, conn).await?
        }

        Some(TwoFactorType::Remember) => {
            match device.twofactor_remember {
                Some(ref code) if !CONFIG.disable_2fa_remember() && ct_eq(code, twofactor_code) => {
                    remember = 1; // Make sure we also return the token here, otherwise it will only remember the first time
                }
                _ => {
                    err_json!(
                        _json_err_twofactor(&twofactor_ids, &user.uuid, data, client_version, conn).await?,
                        "2FA Remember token not provided"
                    )
                }
            }
        }
        _ => err!(
            "Invalid two factor provider",
            ErrorEvent {
                event: EventType::UserFailedLogIn2fa
            }
        ),
    }

    TwoFactorIncomplete::mark_complete(&user.uuid, &device.uuid, conn).await?;

    if !CONFIG.disable_2fa_remember() && remember == 1 {
        Ok(Some(device.refresh_twofactor_remember()))
    } else {
        device.delete_twofactor_remember();
        Ok(None)
    }
}

fn _selected_data(tf: Option<TwoFactor>) -> ApiResult<String> {
    tf.map(|t| t.data).map_res("Two factor doesn't exist")
}

async fn _json_err_twofactor(
    providers: &[i32],
    user_id: &UserId,
    data: &ConnectData,
    client_version: &Option<ClientVersion>,
    conn: &mut DbConn,
) -> ApiResult<Value> {
    let mut result = json!({
        "error" : "invalid_grant",
        "error_description" : "Two factor required.",
        "TwoFactorProviders" : providers.iter().map(ToString::to_string).collect::<Vec<String>>(),
        "TwoFactorProviders2" : {}, // { "0" : null }
        "MasterPasswordPolicy": {
            "Object": "masterPasswordPolicy"
        }
    });

    for provider in providers {
        result["TwoFactorProviders2"][provider.to_string()] = Value::Null;

        match TwoFactorType::from_i32(*provider) {
            Some(TwoFactorType::Authenticator) => { /* Nothing to do for TOTP */ }

            Some(TwoFactorType::Webauthn) if CONFIG.domain_set() => {
                let request = webauthn::generate_webauthn_login(user_id, conn).await?;
                result["TwoFactorProviders2"][provider.to_string()] = request.0;
            }

            Some(TwoFactorType::Duo) => {
                let email = match User::find_by_uuid(user_id, conn).await {
                    Some(u) => u.email,
                    None => err!("User does not exist"),
                };

                match CONFIG.duo_use_iframe() {
                    true => {
                        // Legacy iframe prompt flow
                        let (signature, host) = duo::generate_duo_signature(&email, conn).await?;
                        result["TwoFactorProviders2"][provider.to_string()] = json!({
                            "Host": host,
                            "Signature": signature,
                        })
                    }
                    false => {
                        // OIDC based flow
                        let auth_url = duo_oidc::get_duo_auth_url(
                            &email,
                            data.client_id.as_ref().unwrap(),
                            data.device_identifier.as_ref().unwrap(),
                            conn,
                        )
                        .await?;

                        result["TwoFactorProviders2"][provider.to_string()] = json!({
                            "AuthUrl": auth_url,
                        })
                    }
                }
            }

            Some(tf_type @ TwoFactorType::YubiKey) => {
                let Some(twofactor) = TwoFactor::find_by_user_and_type(user_id, tf_type as i32, conn).await else {
                    err!("No YubiKey devices registered")
                };

                let yubikey_metadata: yubikey::YubikeyMetadata = serde_json::from_str(&twofactor.data)?;

                result["TwoFactorProviders2"][provider.to_string()] = json!({
                    "Nfc": yubikey_metadata.nfc,
                })
            }

            Some(tf_type @ TwoFactorType::Email) => {
                let Some(twofactor) = TwoFactor::find_by_user_and_type(user_id, tf_type as i32, conn).await else {
                    err!("No twofactor email registered")
                };

                // Starting with version 2025.5.0 the client will call `/api/two-factor/send-email-login`.
                let disabled_send = if let Some(cv) = client_version {
                    let ver_match = semver::VersionReq::parse(">=2025.5.0").unwrap();
                    ver_match.matches(&cv.0)
                } else {
                    false
                };

                // Send email immediately if email is the only 2FA option.
                if providers.len() == 1 && !disabled_send {
                    email::send_token(user_id, conn).await?
                }

                let email_data = email::EmailTokenData::from_json(&twofactor.data)?;
                result["TwoFactorProviders2"][provider.to_string()] = json!({
                    "Email": email::obscure_email(&email_data.email),
                })
            }

            _ => {}
        }
    }

    Ok(result)
}

#[post("/accounts/prelogin", data = "<data>")]
async fn prelogin(data: Json<PreloginData>, conn: DbConn) -> Json<Value> {
    _prelogin(data, conn).await
}

#[post("/accounts/register", data = "<data>")]
async fn identity_register(data: Json<RegisterData>, conn: DbConn) -> JsonResult {
    _register(data, false, conn).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterVerificationData {
    email: String,
    name: Option<String>,
    // receiveMarketingEmails: bool,
}

#[derive(rocket::Responder)]
enum RegisterVerificationResponse {
    NoContent(()),
    Token(Json<String>),
}

#[post("/accounts/register/send-verification-email", data = "<data>")]
async fn register_verification_email(
    data: Json<RegisterVerificationData>,
    mut conn: DbConn,
) -> ApiResult<RegisterVerificationResponse> {
    let data = data.into_inner();

    // the registration can only continue if signup is allowed or there exists an invitation
    if !(CONFIG.is_signup_allowed(&data.email)
        || (!CONFIG.mail_enabled() && Invitation::find_by_mail(&data.email, &mut conn).await.is_some()))
    {
        err!("Registration not allowed or user already exists")
    }

    let should_send_mail = CONFIG.mail_enabled() && CONFIG.signups_verify();

    let token_claims =
        crate::auth::generate_register_verify_claims(data.email.clone(), data.name.clone(), should_send_mail);
    let token = crate::auth::encode_jwt(&token_claims);

    if should_send_mail {
        let user = User::find_by_mail(&data.email, &mut conn).await;
        if user.filter(|u| u.private_key.is_some()).is_some() {
            // There is still a timing side channel here in that the code
            // paths that send mail take noticeably longer than ones that
            // don't. Add a randomized sleep to mitigate this somewhat.
            use rand::{rngs::SmallRng, Rng, SeedableRng};
            let mut rng = SmallRng::from_os_rng();
            let delta: i32 = 100;
            let sleep_ms = (1_000 + rng.random_range(-delta..=delta)) as u64;
            tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
        } else {
            mail::send_register_verify_email(&data.email, &token).await?;
        }

        Ok(RegisterVerificationResponse::NoContent(()))
    } else {
        // If email verification is not required, return the token directly
        // the clients will use this token to finish the registration
        Ok(RegisterVerificationResponse::Token(Json(token)))
    }
}

#[post("/accounts/register/finish", data = "<data>")]
async fn register_finish(data: Json<RegisterData>, conn: DbConn) -> JsonResult {
    _register(data, true, conn).await
}

// https://github.com/bitwarden/jslib/blob/master/common/src/models/request/tokenRequest.ts
// https://github.com/bitwarden/mobile/blob/master/src/Core/Models/Request/TokenRequest.cs
#[derive(Debug, Clone, Default, FromForm)]
struct ConnectData {
    #[field(name = uncased("grant_type"))]
    #[field(name = uncased("granttype"))]
    grant_type: String, // refresh_token, password, client_credentials (API key)

    // Needed for grant_type="refresh_token"
    #[field(name = uncased("refresh_token"))]
    #[field(name = uncased("refreshtoken"))]
    refresh_token: Option<String>,

    // Needed for grant_type = "password" | "client_credentials"
    #[field(name = uncased("client_id"))]
    #[field(name = uncased("clientid"))]
    client_id: Option<String>, // web, cli, desktop, browser, mobile
    #[field(name = uncased("client_secret"))]
    #[field(name = uncased("clientsecret"))]
    client_secret: Option<String>,
    #[field(name = uncased("password"))]
    password: Option<String>,
    #[field(name = uncased("scope"))]
    scope: Option<String>,
    #[field(name = uncased("username"))]
    username: Option<String>,

    #[field(name = uncased("device_identifier"))]
    #[field(name = uncased("deviceidentifier"))]
    device_identifier: Option<DeviceId>,
    #[field(name = uncased("device_name"))]
    #[field(name = uncased("devicename"))]
    device_name: Option<String>,
    #[field(name = uncased("device_type"))]
    #[field(name = uncased("devicetype"))]
    device_type: Option<String>,
    #[allow(unused)]
    #[field(name = uncased("device_push_token"))]
    #[field(name = uncased("devicepushtoken"))]
    _device_push_token: Option<String>, // Unused; mobile device push not yet supported.

    // Needed for two-factor auth
    #[field(name = uncased("two_factor_provider"))]
    #[field(name = uncased("twofactorprovider"))]
    two_factor_provider: Option<i32>,
    #[field(name = uncased("two_factor_token"))]
    #[field(name = uncased("twofactortoken"))]
    two_factor_token: Option<String>,
    #[field(name = uncased("two_factor_remember"))]
    #[field(name = uncased("twofactorremember"))]
    two_factor_remember: Option<i32>,
    #[field(name = uncased("authrequest"))]
    auth_request: Option<AuthRequestId>,
}

fn _check_is_some<T>(value: &Option<T>, msg: &str) -> EmptyResult {
    if value.is_none() {
        err!(msg)
    }
    Ok(())
}
