class SiteController < ApplicationController
  skip_forgery_protection only: :create_contact_request

  def home
    load_homepage_state
  end

  def create_contact_request
    load_homepage_state
    @contact_email = params.dig(:contact_request, :email).to_s.strip

    if @contact_email.match?(URI::MailTo::EMAIL_REGEXP)
      ContactMailer.contact_confirmation(@contact_email).deliver_now!
      redirect_to root_path(anchor: "pricing"), notice: "Thanks. Check your inbox for a confirmation email."
    else
      @show_contact_form = true
      flash.now[:alert] = "Enter a valid email address."
      render :home, status: :unprocessable_entity
    end
  rescue StandardError
    @show_contact_form = true
    flash.now[:alert] = "We couldn't send the confirmation email yet. Email delivery still needs to be configured."
    render :home, status: :service_unavailable
  end

  private

  def load_homepage_state
    @mac_download_url = ENV["HEADROOM_MAC_DOWNLOAD_URL"]
    @contact_email ||= ""
    @show_contact_form ||= false
  end
end
