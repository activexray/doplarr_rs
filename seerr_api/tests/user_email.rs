use seerr_api::models::{MediaRequest, TvDetails, User, UserGet200Response};
use serde_json::{json, Value};

fn tv_details() -> Value {
    json!({
        "id": 42,
        "seasons": [{"seasonNumber": 1}, {"seasonNumber": 2}],
        "mediaInfo": {
            "status": 4,
            "requests": [{
                "id": 1,
                "status": 2,
                "requestedBy": {
                    "id": 1,
                    "createdAt": "2000-01-01T00:00:00Z",
                    "updatedAt": "2000-01-01T00:00:00Z"
                },
                "modifiedBy": {
                    "id": 2,
                    "createdAt": "2000-01-01T00:00:00Z",
                    "updatedAt": "2000-01-01T00:00:00Z"
                }
            }]
        }
    })
}

#[test]
fn tv_details_accepts_missing_embedded_user_email() {
    for (requester_has_email, modifier_has_email) in [(false, true), (true, false), (false, false)]
    {
        let mut response = tv_details();
        let request = &mut response["mediaInfo"]["requests"][0];
        if requester_has_email {
            request["requestedBy"]["email"] = json!("requester@example.invalid");
        }
        if modifier_has_email {
            request["modifiedBy"]["email"] = json!("modifier@example.invalid");
        }

        let details: TvDetails = serde_json::from_value(response).unwrap();
        assert_eq!(details.seasons.unwrap().len(), 2);
        let requests = details.media_info.unwrap().requests.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].status, 2.0);
        assert_eq!(
            requests[0].requested_by.as_ref().unwrap().email,
            if requester_has_email {
                "requester@example.invalid"
            } else {
                ""
            }
        );
        assert_eq!(
            requests[0].modified_by.as_ref().unwrap().email,
            if modifier_has_email {
                "modifier@example.invalid"
            } else {
                ""
            }
        );
    }
}

#[test]
fn media_request_preserves_supplied_user_emails() {
    let mut response = tv_details();
    let request = &mut response["mediaInfo"]["requests"][0];
    request["requestedBy"]["email"] = json!("requester@example.invalid");
    request["modifiedBy"]["email"] = json!("modifier@example.invalid");
    let parsed: MediaRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(
        parsed.requested_by.unwrap().email,
        "requester@example.invalid"
    );
    assert_eq!(
        parsed.modified_by.unwrap().email,
        "modifier@example.invalid"
    );
}

#[test]
fn direct_user_and_user_list_accept_missing_email() {
    let response = tv_details();
    let user = response["mediaInfo"]["requests"][0]["requestedBy"].clone();
    let parsed: User = serde_json::from_value(user.clone()).unwrap();
    assert_eq!(parsed.id, 1);
    assert!(parsed.email.is_empty());

    let page: UserGet200Response = serde_json::from_value(json!({"results": [user]})).unwrap();
    assert!(page.results.unwrap()[0].email.is_empty());
}

#[test]
fn missing_email_does_not_relax_other_user_fields() {
    for field in ["requestedBy", "modifiedBy"] {
        for required in ["id", "createdAt", "updatedAt"] {
            let mut response = tv_details();
            let request = &mut response["mediaInfo"]["requests"][0];
            request["requestedBy"]["email"] = json!("requester@example.invalid");
            request["modifiedBy"]["email"] = json!("modifier@example.invalid");
            request[field].as_object_mut().unwrap().remove(required);
            assert!(serde_json::from_value::<TvDetails>(response).is_err());
        }
    }
}

#[test]
fn embedded_user_email_still_requires_a_string_when_present() {
    for field in ["requestedBy", "modifiedBy"] {
        let mut response = tv_details();
        let request = &mut response["mediaInfo"]["requests"][0];
        request["requestedBy"]["email"] = json!("requester@example.invalid");
        request["modifiedBy"]["email"] = json!("modifier@example.invalid");
        request[field]["email"] = json!(123);
        assert!(serde_json::from_value::<TvDetails>(response).is_err());
    }
}
