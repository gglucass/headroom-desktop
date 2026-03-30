require "test_helper"

class SiteControllerTest < ActionDispatch::IntegrationTest
  setup do
    ActionMailer::Base.deliveries.clear
  end

  test "home renders" do
    get root_url
    assert_response :success
    assert_select "h1", /Headroom cuts Claude Code costs/
    assert_select "body", /Download for macOS/
  end

  test "contact request sends confirmation email" do
    assert_difference -> { ActionMailer::Base.deliveries.size }, 1 do
      post contact_request_url, params: { contact_request: { email: "founder@example.com" } }
    end

    assert_redirected_to root_url(anchor: "pricing")

    mail = ActionMailer::Base.deliveries.last
    assert_equal ["founder@example.com"], mail.to
    assert_equal [ENV.fetch("HEADROOM_CONTACT_FROM_EMAIL", "hello@example.com")], mail.from
    assert_equal "Thank you for contacting Headroom", mail.subject
    assert_includes mail.body.encoded, "We'll reach out shortly."
  end

  test "contact request with invalid email rerenders home" do
    assert_no_difference -> { ActionMailer::Base.deliveries.size } do
      post contact_request_url, params: { contact_request: { email: "not-an-email" } }
    end

    assert_response :unprocessable_entity
    assert_select ".flash-alert", /Enter a valid email address/
    assert_select "form.contact-request__form"
  end
end
